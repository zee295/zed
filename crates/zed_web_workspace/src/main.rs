//! Browser entry: real Zed workspace UI on WASM.
//!
//! Wires the desktop init surface that compiles for wasm: workspace chrome,
//! panels, status bar, title bar, keymaps, menus, language registry (configs
//! without native grammars), theme/tasks/snippets/etc.

#![cfg_attr(target_family = "wasm", no_main)]

use gpui::{
    App, AppContext as _, AssetSource, Entity, Focusable, SharedString, Subscription, TaskExt as _,
    TitlebarOptions, WeakEntity, Window, WindowKind, WindowOptions, point, px,
};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use theme::ActiveTheme;
use util::ResultExt as _;

#[cfg(target_family = "wasm")]
mod remote_highlight;

#[cfg(target_family = "wasm")]
mod host_debug_adapter;

#[cfg(target_family = "wasm")]
mod web_menu_bar;

#[cfg(target_family = "wasm")]
mod web_user_menu;

#[cfg(target_family = "wasm")]
mod web_agent_panel;

#[cfg(target_family = "wasm")]
mod web_proxy_http;

#[cfg(target_family = "wasm")]
mod web_settings_modal;

#[cfg(target_family = "wasm")]
mod web_quick_action_bar;

#[cfg(target_family = "wasm")]
use wasm_bindgen::prelude::*;

#[cfg(target_family = "wasm")]
#[wasm_bindgen(inline_js = r#"
export async function zedFetchAssetPack(url) {
    const response = await fetch(url, { credentials: "same-origin" });
    if (!response.ok) {
        throw new Error(`asset pack HTTP ${response.status}`);
    }
    return new Uint8Array(await response.arrayBuffer());
}

export function zedWorkspacePaths() {
    return JSON.stringify(new URL(self.location.href).searchParams.getAll("path"));
}

export function zedWorkspaceProjectGroups() {
    return new URL(self.location.href).searchParams.get("projects") ?? "[]";
}

export function zedSyncWorkspacePaths(pathsJson) {
    const paths = JSON.parse(pathsJson);
    if (!Array.isArray(paths) || !paths.length) return;
    const url = new URL(self.location.href);
    const current = url.searchParams.getAll("path");
    if (JSON.stringify(current) === JSON.stringify(paths)) return;
    url.searchParams.delete("path");
    for (const path of paths) url.searchParams.append("path", path);
    self.history.replaceState(self.history.state, "", url);
}

export function zedSyncWorkspaceProjectGroups(groupsJson) {
    const groups = JSON.parse(groupsJson);
    if (!Array.isArray(groups) || !groups.length) return;
    const url = new URL(self.location.href);
    if (url.searchParams.get("projects") === groupsJson) return;
    url.searchParams.set("projects", groupsJson);
    self.history.replaceState(self.history.state, "", url);
}

export function zedAgentPanelOpen() {
    try {
        return self.localStorage.getItem("zed-web-agent-panel-open") === "true";
    } catch {
        return false;
    }
}

export function zedSetAgentPanelOpen(open) {
    try {
        self.localStorage.setItem("zed-web-agent-panel-open", open ? "true" : "false");
    } catch {}
}

export function zedWorkspaceSidebarOpen() {
    try {
        return self.localStorage.getItem("zed-web-workspace-sidebar-open") === "true";
    } catch {
        return false;
    }
}

export function zedSetWorkspaceSidebarOpen(open) {
    try {
        self.localStorage.setItem("zed-web-workspace-sidebar-open", open ? "true" : "false");
    } catch {}
}

export function zedOpenExternalUrl(url) {
    return self.__zedOpenExternalUrl?.(url) ?? false;
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = zedFetchAssetPack)]
    fn fetch_asset_pack(url: &str) -> js_sys::Promise;

    #[wasm_bindgen(js_name = zedWorkspacePaths)]
    fn workspace_paths_json() -> String;

    #[wasm_bindgen(js_name = zedWorkspaceProjectGroups)]
    fn workspace_project_groups_json() -> String;

    #[wasm_bindgen(js_name = zedSyncWorkspacePaths)]
    fn sync_workspace_paths_json(paths: &str);

    #[wasm_bindgen(js_name = zedSyncWorkspaceProjectGroups)]
    fn sync_workspace_project_groups_json(groups: &str);

    #[wasm_bindgen(js_name = zedAgentPanelOpen)]
    fn saved_agent_panel_open() -> bool;

    #[wasm_bindgen(js_name = zedSetAgentPanelOpen)]
    fn save_agent_panel_open(open: bool);

    #[wasm_bindgen(js_name = zedWorkspaceSidebarOpen)]
    fn saved_workspace_sidebar_open() -> bool;

    #[wasm_bindgen(js_name = zedSetWorkspaceSidebarOpen)]
    fn save_workspace_sidebar_open(open: bool);

    #[wasm_bindgen(js_name = zedOpenExternalUrl)]
    fn open_external_url(url: &str) -> bool;
}

#[cfg(target_family = "wasm")]
const WORKSPACE_ROOT: &str = "/workspace";

#[cfg(target_family = "wasm")]
fn workspace_paths_from_url() -> Vec<std::path::PathBuf> {
    serde_json::from_str::<Vec<String>>(&workspace_paths_json())
        .unwrap_or_default()
        .into_iter()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .collect()
}

#[cfg(target_family = "wasm")]
fn workspace_project_groups_from_url() -> Vec<Vec<std::path::PathBuf>> {
    serde_json::from_str::<Vec<Vec<String>>>(&workspace_project_groups_json())
        .unwrap_or_default()
        .into_iter()
        .map(|paths| {
            paths
                .into_iter()
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>()
        })
        .filter(|paths| !paths.is_empty())
        .collect()
}

#[cfg(target_family = "wasm")]
fn sync_project_paths_url(project: &Entity<project::Project>, cx: &App) {
    let paths = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().display().to_string())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return;
    }
    if let Ok(paths) = serde_json::to_string(&paths) {
        sync_workspace_paths_json(&paths);
    }
}

#[cfg(target_family = "wasm")]
fn sync_project_groups_url(groups: &[project::ProjectGroupKey]) {
    let groups = groups
        .iter()
        .filter(|group| group.host().is_none())
        .map(|group| {
            group
                .path_list()
                .ordered_paths()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|paths| !paths.is_empty())
        .collect::<Vec<_>>();
    if let Ok(groups) = serde_json::to_string(&groups) {
        sync_workspace_project_groups_json(&groups);
    }
}

#[cfg(target_family = "wasm")]
fn multi_workspace_project_groups(
    multi_workspace: &workspace::MultiWorkspace,
    cx: &App,
) -> Vec<project::ProjectGroupKey> {
    let mut groups = multi_workspace.project_group_keys();
    for workspace in multi_workspace.workspaces() {
        let key = workspace.read(cx).project_group_key(cx);
        if !groups.contains(&key) {
            groups.push(key);
        }
    }
    groups
}

#[cfg(target_family = "wasm")]
async fn load_web_assets() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Read as _;
    use wasm_bindgen::JsCast as _;

    let value = wasm_bindgen_futures::JsFuture::from(fetch_asset_pack("/zed-assets.tar"))
        .await
        .map_err(|error| anyhow::anyhow!("fetch web asset pack: {error:?}"))?;
    let bytes = value
        .dyn_into::<js_sys::Uint8Array>()
        .map_err(|_| anyhow::anyhow!("web asset pack response is not a byte array"))?
        .to_vec();
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut assets = BTreeMap::new();
    for entry in archive.entries().context("read web asset pack")? {
        let mut entry = entry.context("read web asset entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .context("read web asset path")?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        let mut content = Vec::new();
        entry
            .read_to_end(&mut content)
            .with_context(|| format!("read web asset {path}"))?;
        assets.insert(path, content);
    }
    assets::install_web_assets(assets)?;
    Ok(())
}

#[cfg(target_family = "wasm")]
#[derive(Clone)]
struct WebAssets {
    extension_assets: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

#[cfg(target_family = "wasm")]
impl AssetSource for WebAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = self
            .extension_assets
            .read()
            .expect("extension asset map lock poisoned")
            .get(path)
            .cloned()
        {
            return Ok(Some(Cow::Owned(bytes)));
        }
        AssetSource::load(&assets::Assets, path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut paths = AssetSource::list(&assets::Assets, path)?;
        paths.extend(
            self.extension_assets
                .read()
                .expect("extension asset map lock poisoned")
                .keys()
                .filter(|asset_path| asset_path.starts_with(path))
                .cloned()
                .map(SharedString::from),
        );
        Ok(paths)
    }
}

#[cfg(target_family = "wasm")]
gpui::actions!(
    zed_web_workspace,
    [
        /// Opens files from the host-backed workspace.
        OpenRemoteFiles,
        /// Opens a folder from the host-backed workspace.
        OpenRemoteFolder
    ]
);

#[cfg(target_family = "wasm")]
fn web_window_options(_display: Option<uuid::Uuid>, cx: &mut App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.0), px(9.0))),
        }),
        window_bounds: None,
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        app_owns_titlebar_drag: true,
        window_background: cx.theme().window_background_appearance(),
        app_id: Some("zed-web".into()),
        window_min_size: Some(gpui::Size {
            width: px(360.0),
            height: px(240.0),
        }),
        ..Default::default()
    }
}

/// Register common languages without native tree-sitter grammars so file-type
/// detection, comment toggles, and language selectors work in the browser.
///
/// LSP adapters are host-process shims: binaries run on the server via
/// `Process::*` (same stack LanguageServer uses for stdio JSON-RPC).
#[cfg(target_family = "wasm")]
fn register_plain_languages(languages: &language::LanguageRegistry) {
    use language::{LanguageConfig, LanguageMatcher, LanguageName, LoadedLanguage};
    use std::sync::Arc;

    // Names must match desktop language ids where features look them up by name
    // (e.g. BufferSearchBar uses language_for_name("regex")).
    let specs: &[(&str, &[&str], &[&str])] = &[
        ("Plain Text", &[], &[]),
        ("regex", &[], &[]),
        ("jsdoc", &[], &[]),
        ("Git Commit", &[], &["#"]),
        ("Rust", &["rs"], &["//", "///"]),
        ("Python", &["py", "pyi"], &["#"]),
        ("JavaScript", &["js", "mjs", "cjs", "jsx"], &["//"]),
        ("TypeScript", &["ts", "tsx", "mts", "cts"], &["//"]),
        ("JSON", &["json", "jsonc"], &[]),
        ("Markdown", &["md", "mdx", "markdown"], &[]),
        ("TOML", &["toml"], &["#"]),
        ("YAML", &["yml", "yaml"], &["#"]),
        ("HTML", &["html", "htm"], &[]),
        ("CSS", &["css", "scss", "sass", "less"], &["//"]),
        ("Go", &["go"], &["//"]),
        ("C", &["c", "h"], &["//"]),
        ("C++", &["cpp", "cc", "cxx", "hpp", "hxx"], &["//"]),
        ("Shell Script", &["sh", "bash", "zsh"], &["#"]),
        ("SQL", &["sql"], &["--"]),
        ("Ruby", &["rb"], &["#"]),
        ("Java", &["java"], &["//"]),
        ("Swift", &["swift"], &["//"]),
        ("Kotlin", &["kt", "kts"], &["//"]),
        ("Dockerfile", &["Dockerfile"], &["#"]),
        ("Makefile", &["Makefile", "makefile", "mk"], &["#"]),
    ];

    for (name, suffixes, line_comments) in specs {
        let name = LanguageName::new(name);
        let suffixes: Vec<String> = suffixes.iter().map(|s| (*s).to_string()).collect();
        let line_comments: Vec<Arc<str>> = line_comments.iter().map(|c| Arc::from(*c)).collect();
        let config = LanguageConfig {
            name: name.clone(),
            matcher: LanguageMatcher {
                path_suffixes: suffixes,
                first_line_pattern: None,
                ..LanguageMatcher::default()
            },
            line_comments,
            ..LanguageConfig::default()
        };
        languages.register_language(
            name,
            None,
            config.matcher.clone(),
            false,
            None,
            Arc::new(move || {
                Ok(LoadedLanguage {
                    config: config.clone(),
                    queries: Default::default(),
                    context_provider: None,
                    toolchain_provider: None,
                    manifest_name: None,
                })
            }),
        );
    }

    // Host-side language servers (spawned on the remote via Process::spawn).
    languages.register_lsp_adapter(
        LanguageName::new("Rust"),
        Arc::new(HostLspAdapter::rust_analyzer()),
    );

    web_sys::console::log_1(
        &format!(
            "zed_web_workspace: registered {} plain languages + host LSP (rust-analyzer)",
            specs.len()
        )
        .into(),
    );
}

/// Minimal LSP adapter: use a binary already installed on the server host.
/// No download path — Process::spawn runs the binary server-side.
#[cfg(target_family = "wasm")]
struct HostLspAdapter {
    name: &'static str,
    program: &'static str,
    disk_based_diagnostics_sources: Vec<String>,
    disk_based_diagnostics_progress_token: Option<String>,
}

#[cfg(target_family = "wasm")]
impl HostLspAdapter {
    fn rust_analyzer() -> Self {
        Self {
            name: "rust-analyzer",
            program: "rust-analyzer",
            disk_based_diagnostics_sources: vec!["rustc".into()],
            disk_based_diagnostics_progress_token: Some("rust-analyzer/flycheck".into()),
        }
    }
}

#[cfg(target_family = "wasm")]
impl language::LspInstaller for HostLspAdapter {
    type BinaryVersion = ();

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn language::LspAdapterDelegate>,
        _: Option<language::Toolchain>,
        _: &gpui::AsyncApp,
    ) -> Option<lsp::LanguageServerBinary> {
        // Prefer remote `which` if the delegate can resolve it; otherwise pass
        // the bare program name so the server Process::spawn PATH lookup runs.
        let path = delegate
            .which(std::ffi::OsStr::new(self.program))
            .await
            .unwrap_or_else(|| std::path::PathBuf::from(self.program));
        let env = delegate.shell_env().await;
        Some(lsp::LanguageServerBinary {
            path,
            arguments: Vec::new(),
            env: Some(env),
        })
    }

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn language::LspAdapterDelegate>,
        _: bool,
        _: &mut gpui::AsyncApp,
    ) -> anyhow::Result<Self::BinaryVersion> {
        anyhow::bail!(
            "host LSP adapter for {} does not download binaries; install {} on the server",
            self.name,
            self.program
        )
    }

    fn fetch_server_binary(
        &self,
        _: (),
        _: std::path::PathBuf,
        _: &Arc<dyn language::LspAdapterDelegate>,
    ) -> impl std::future::Future<Output = anyhow::Result<lsp::LanguageServerBinary>> + Send + use<>
    {
        let name = self.name;
        let program = self.program;
        async move {
            anyhow::bail!(
                "host LSP adapter for {name} does not download binaries; install {program} on the server"
            )
        }
    }

    async fn cached_server_binary(
        &self,
        _: std::path::PathBuf,
        _: &dyn language::LspAdapterDelegate,
    ) -> Option<lsp::LanguageServerBinary> {
        None
    }
}

#[cfg(target_family = "wasm")]
#[async_trait::async_trait(?Send)]
impl language::LspAdapter for HostLspAdapter {
    fn name(&self) -> lsp::LanguageServerName {
        lsp::LanguageServerName::new_static(self.name)
    }

    fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        self.disk_based_diagnostics_sources.clone()
    }

    fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        self.disk_based_diagnostics_progress_token.clone()
    }
}

#[cfg(target_family = "wasm")]
fn load_keymaps(cx: &mut App) {
    use settings::{
        BaseKeymap, KeybindSource, KeymapFile, Settings, default_keymap_path,
        specific_overrides_keymap_path,
    };

    // Use allow_partial_failure: the full desktop keymap references agent/vim/…
    // actions that are not compiled into the web binary. Strict load_asset
    // would reject the entire file and leave zero bindings (backspace/arrows
    // appear "broken"). Partial load still registers editor/workspace chords.
    let mut total = 0usize;

    let default_keymap_path = default_keymap_path();
    match KeymapFile::load_asset_allow_partial_failure(default_keymap_path, cx) {
        Ok(mut bindings) => {
            for b in &mut bindings {
                b.set_meta(KeybindSource::Default.meta());
            }
            total += bindings.len();
            cx.bind_keys(bindings);
        }
        Err(err) => web_sys::console::error_1(
            &format!("zed_web_workspace: default keymap failed: {err:#}").into(),
        ),
    }

    let base = *BaseKeymap::get_global(cx);
    if let Some(asset_path) = base.asset_path() {
        match KeymapFile::load_asset_allow_partial_failure(asset_path, cx) {
            Ok(mut bindings) => {
                for b in &mut bindings {
                    b.set_meta(KeybindSource::Base.meta());
                }
                total += bindings.len();
                cx.bind_keys(bindings);
            }
            Err(err) => web_sys::console::warn_1(
                &format!("zed_web_workspace: base keymap {asset_path}: {err:#}").into(),
            ),
        }
    }

    let overrides_keymap_path = specific_overrides_keymap_path();
    match KeymapFile::load_asset_allow_partial_failure(overrides_keymap_path, cx) {
        Ok(mut bindings) => {
            for b in &mut bindings {
                b.set_meta(KeybindSource::Default.meta());
            }
            total += bindings.len();
            cx.bind_keys(bindings);
        }
        Err(err) => web_sys::console::warn_1(
            &format!("zed_web_workspace: overrides keymap: {err:#}").into(),
        ),
    }

    web_sys::console::log_1(
        &format!("zed_web_workspace: keymaps loaded ({total} bindings)").into(),
    );
}

#[cfg(target_family = "wasm")]
fn watch_user_keymap(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    use futures::StreamExt as _;
    use settings::{KeybindSource, KeymapFile, KeymapFileLoadResult};

    let (mut changes, watcher) =
        settings::watch_config_file(&cx.background_executor(), fs, paths::keymap_file().clone());
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(content) = changes.next().await {
            cx.update(|cx| {
                let mut user_bindings = match KeymapFile::load(&content, cx) {
                    KeymapFileLoadResult::Success { key_bindings } => key_bindings,
                    KeymapFileLoadResult::SomeFailedToLoad {
                        key_bindings,
                        error_message,
                    } => {
                        log::error!("some user key bindings failed to load: {error_message}");
                        if key_bindings.is_empty() {
                            return;
                        }
                        key_bindings
                    }
                    KeymapFileLoadResult::JsonParseFailure { error } => {
                        log::error!("failed to parse user keymap: {error:#}");
                        return;
                    }
                };

                cx.clear_key_bindings();
                load_keymaps(cx);
                for binding in &mut user_bindings {
                    binding.set_meta(KeybindSource::User.meta());
                }
                cx.bind_keys(user_bindings);
                keymap_editor::KeymapEventChannel::trigger_keymap_changed(cx);
            });
        }
    })
    .detach();
}

#[cfg(target_family = "wasm")]
fn install_menus(cx: &mut App) {
    use editor::actions::{
        AddSelectionAbove, AddSelectionBelow, Copy, CopyAndTrim, Cut, DuplicateLineDown,
        FindAllReferences, GoToDeclaration, GoToDefinition, GoToDiagnostic, GoToPreviousDiagnostic,
        GoToTypeDefinition, MoveLineDown, MoveLineUp, Paste, Redo, SelectAll, SelectAllMatches,
        SelectLargerSyntaxNode, SelectNext, SelectNextSyntaxNode, SelectPrevious,
        SelectPreviousSyntaxNode, SelectSmallerSyntaxNode, ToggleComments, ToggleGoToLine, Undo,
    };
    use gpui::{Menu, MenuItem};
    use outline_panel::ToggleFocus as OutlinePanelToggle;
    use terminal_view::terminal_panel::Toggle as TerminalToggle;
    use workspace::{
        CloseActiveItem, DeploySearch, GoBack, GoForward, NewFile, Save, SaveAll, SaveAs,
        SplitDown, SplitLeft, SplitRight, SplitUp, ToggleAllDocks, ToggleBottomDock,
        ToggleFileFinder, ToggleLeftDock, ToggleRightDock,
    };

    // Browser has no OS menu bar — these feed the in-window WebMenuBar (same
    // pattern as desktop Linux/Windows ApplicationMenu).
    cx.set_menus(vec![
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New File", NewFile),
                MenuItem::action("Open File…", OpenRemoteFiles),
                MenuItem::action("Open Folder…", OpenRemoteFolder),
                MenuItem::separator(),
                MenuItem::action("Save", Save { save_intent: None }),
                MenuItem::action("Save As…", SaveAs),
                MenuItem::action("Save All", SaveAll { save_intent: None }),
                MenuItem::separator(),
                MenuItem::action(
                    "Close Editor",
                    CloseActiveItem {
                        save_intent: None,
                        close_pinned: false,
                    },
                ),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Undo", Undo),
                MenuItem::action("Redo", Redo),
                MenuItem::separator(),
                MenuItem::action("Cut", Cut),
                MenuItem::action("Copy", Copy),
                MenuItem::action("Copy and Trim", CopyAndTrim),
                MenuItem::action("Paste", Paste),
                MenuItem::separator(),
                MenuItem::action("Find", search::buffer_search::Deploy::find()),
                MenuItem::action("Find in Project", DeploySearch::default()),
                MenuItem::separator(),
                MenuItem::action("Toggle Line Comment", ToggleComments::default()),
            ],
        },
        Menu {
            name: "Selection".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Select All", SelectAll),
                MenuItem::action("Expand Selection", SelectLargerSyntaxNode),
                MenuItem::action("Shrink Selection", SelectSmallerSyntaxNode),
                MenuItem::action("Select Next Sibling", SelectNextSyntaxNode),
                MenuItem::action("Select Previous Sibling", SelectPreviousSyntaxNode),
                MenuItem::separator(),
                MenuItem::action(
                    "Add Cursor Above",
                    AddSelectionAbove {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    "Add Cursor Below",
                    AddSelectionBelow {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    "Select Next Occurrence",
                    SelectNext {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    "Select Previous Occurrence",
                    SelectPrevious {
                        replace_newest: false,
                    },
                ),
                MenuItem::action("Select All Occurrences", SelectAllMatches),
                MenuItem::separator(),
                MenuItem::action("Move Line Up", MoveLineUp),
                MenuItem::action("Move Line Down", MoveLineDown),
                MenuItem::action("Duplicate Selection", DuplicateLineDown),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    "Zoom In",
                    zed_actions::IncreaseBufferFontSize { persist: false },
                ),
                MenuItem::action(
                    "Zoom Out",
                    zed_actions::DecreaseBufferFontSize { persist: false },
                ),
                MenuItem::action(
                    "Reset Zoom",
                    zed_actions::ResetBufferFontSize { persist: false },
                ),
                MenuItem::separator(),
                MenuItem::action("Toggle Left Dock", ToggleLeftDock),
                MenuItem::action("Toggle Right Dock", ToggleRightDock),
                MenuItem::action("Toggle Bottom Dock", ToggleBottomDock),
                MenuItem::action("Toggle All Docks", ToggleAllDocks),
                MenuItem::submenu(Menu {
                    name: "Editor Layout".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("Split Up", SplitUp::default()),
                        MenuItem::action("Split Down", SplitDown::default()),
                        MenuItem::action("Split Left", SplitLeft::default()),
                        MenuItem::action("Split Right", SplitRight::default()),
                    ],
                }),
                MenuItem::separator(),
                MenuItem::action("Project Panel", zed_actions::project_panel::ToggleFocus),
                MenuItem::action("Outline Panel", OutlinePanelToggle),
                MenuItem::action("Terminal Panel", TerminalToggle),
                MenuItem::action("Git Panel", zed_actions::git_panel::ToggleFocus),
                MenuItem::action("Debugger Panel", zed_actions::debug_panel::ToggleFocus),
                MenuItem::action("Agent Panel", zed_actions::assistant::ToggleFocus),
                // Real desktop threads sidebar (same as desktop `cmd-alt-j`;
                // keyboard chords don't dispatch in the browser, so expose it
                // in the menu where clicks do work).
                MenuItem::action("Agent Sidebar", workspace::ToggleWorkspaceSidebar),
                MenuItem::separator(),
                MenuItem::action("Diagnostics", diagnostics::Deploy),
                MenuItem::separator(),
                MenuItem::action("Command Palette", zed_actions::command_palette::Toggle),
                MenuItem::action("File Finder", ToggleFileFinder::default()),
                MenuItem::action("Project Search", DeploySearch::default()),
                MenuItem::separator(),
                MenuItem::action(
                    "Select Theme…",
                    zed_actions::theme_selector::Toggle {
                        themes_filter: None,
                    },
                ),
                MenuItem::action("Settings", zed_actions::OpenSettings),
                MenuItem::action("Keymap", zed_actions::OpenKeymap),
                MenuItem::action("Extensions", zed_actions::Extensions::default()),
            ],
        },
        Menu {
            name: "Debug".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Start Debugger", debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action("Continue", debugger_ui::Continue),
                MenuItem::action("Pause", debugger_ui::Pause),
                MenuItem::action("Restart", debugger_ui::Restart),
                MenuItem::action("Stop", debugger_ui::Stop),
                MenuItem::separator(),
                MenuItem::action("Step Over", debugger_ui::StepOver),
                MenuItem::action("Step Into", debugger_ui::StepInto),
                MenuItem::action("Step Out", debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action("Clear All Breakpoints", debugger_ui::ClearAllBreakpoints),
            ],
        },
        Menu {
            name: "Go".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Back", GoBack),
                MenuItem::action("Forward", GoForward),
                MenuItem::separator(),
                MenuItem::action("Command Palette…", zed_actions::command_palette::Toggle),
                MenuItem::separator(),
                MenuItem::action("Go to File…", ToggleFileFinder::default()),
                MenuItem::action(
                    "Go to Symbol in Editor…",
                    zed_actions::outline::ToggleOutline,
                ),
                MenuItem::action("Go to Line/Column…", ToggleGoToLine),
                MenuItem::separator(),
                MenuItem::action("Go to Definition", GoToDefinition::default()),
                MenuItem::action("Go to Declaration", GoToDeclaration),
                MenuItem::action("Go to Type Definition", GoToTypeDefinition),
                MenuItem::action("Find All References", FindAllReferences::default()),
                MenuItem::separator(),
                MenuItem::action("Next Problem", GoToDiagnostic::default()),
                MenuItem::action("Previous Problem", GoToPreviousDiagnostic::default()),
            ],
        },
        Menu {
            name: "Terminal".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New Terminal", TerminalToggle),
                MenuItem::action("Toggle Terminal Panel", ToggleBottomDock),
            ],
        },
        Menu {
            name: "Help".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Command Palette…", zed_actions::command_palette::Toggle),
                MenuItem::separator(),
                MenuItem::action(
                    "Select Theme…",
                    zed_actions::theme_selector::Toggle {
                        themes_filter: None,
                    },
                ),
            ],
        },
    ]);
    web_sys::console::log_1(&"zed_web_workspace: app menus registered (in-window menu bar)".into());
}

#[cfg(target_family = "wasm")]
fn install_web_open_actions(cx: &mut App) {
    use gpui::PathPromptOptions;

    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        workspace
            .register_action(|workspace, _: &OpenRemoteFiles, window, cx| {
                let app_state = workspace::AppState::global(cx);
                workspace::prompt_for_open_path_and_open(
                    workspace,
                    app_state,
                    PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: true,
                        prompt: Some("Open file from server".into()),
                    },
                    false,
                    window,
                    cx,
                );
            })
            .register_action(|workspace, _: &OpenRemoteFolder, window, cx| {
                let app_state = workspace::AppState::global(cx);
                workspace::prompt_for_open_path_and_open(
                    workspace,
                    app_state,
                    PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some("Open folder from server".into()),
                    },
                    false,
                    window,
                    cx,
                );
            });
    })
    .detach();
}

#[cfg(target_family = "wasm")]
fn merge_json_defaults(target: &mut serde_json::Value, overrides: serde_json::Value) {
    match (target, overrides) {
        (serde_json::Value::Object(target), serde_json::Value::Object(overrides)) => {
            for (key, value) in overrides {
                if let Some(target_value) = target.get_mut(&key) {
                    merge_json_defaults(target_value, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (target, value) => *target = value,
    }
}

#[cfg(target_family = "wasm")]
fn web_default_settings() -> String {
    let mut defaults = settings::parse_json_with_comments::<serde_json::Value>(
        settings::default_settings().as_ref(),
    )
    .expect("bundled default settings must be valid");
    let overrides = serde_json::from_str(
        r#"{
            "theme": "One Dark",
            "ui_font_size": 14,
            "ui_font_fallbacks": ["Noto Sans Symbols 2", "Symbols Nerd Font Mono"],
            "buffer_font_size": 13,
            "buffer_font_fallbacks": ["Noto Sans Symbols 2", "Symbols Nerd Font Mono"],
            "base_keymap": "VSCode",
            "telemetry": {
                "diagnostics": false,
                "metrics": false
            },
            "use_system_path_prompts": false,
            "use_system_prompts": false,
            "project_panel": {
                "dock": "left",
                "auto_fold_dirs": false,
                "default_width": 280,
                "button": true
            },
            "outline_panel": {
                "dock": "right",
                "default_width": 260,
                "button": true
            },
            "git_panel": {
                "dock": "left",
                "button": true
            },
            "terminal": {
                "dock": "bottom",
                "default_height": 280,
                "button": true,
                "font_fallbacks": ["Noto Sans Symbols 2", "Symbols Nerd Font Mono"]
            },
            "tab_bar": { "show": true, "show_nav_history_buttons": true },
            "toolbar": {
                "breadcrumbs": true,
                "quick_actions": true,
                "selections_menu": true
            },
            "status_bar": { "show": true },
            "title_bar": { "show_branch_name": true, "show_project_items": true },
            "vim_mode": false
        }"#,
    )
    .expect("web default settings overrides must be valid");
    merge_json_defaults(&mut defaults, overrides);
    serde_json::to_string(&defaults).expect("web default settings must serialize")
}

#[cfg(target_family = "wasm")]
fn load_user_settings(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    cx.spawn(async move |cx| {
        match settings::SettingsStore::load_settings(&fs).await {
            Ok(content) => {
                cx.update_global(|store: &mut settings::SettingsStore, cx| {
                    let result = store.set_user_settings(&content, cx);
                    if let settings::ParseStatus::Failed { error } = result.parse_status {
                        log::error!("failed to parse remote user settings: {error}");
                    }
                    cx.refresh_windows();
                });
            }
            Err(error) => log::error!("failed to load remote user settings: {error:#}"),
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(target_family = "wasm")]
fn init_app_state(
    cx: &mut App,
    extension_assets: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
    remote_client: wasm_remote::RemoteClient,
) -> Arc<workspace::AppState> {
    use client::Client;
    use clock::RealSystemClock;
    use fs::Fs;
    use http_client::{BlockedHttpClient, HttpClientWithUrl};
    use node_runtime::NodeRuntime;
    use settings::SettingsStore;
    use wasm_remote::RemoteFs;
    use workspace::{AppState, WorkspaceStore};

    let location = web_sys::window()
        .expect("browser window unavailable")
        .location();
    let server_origin = location.origin().expect("browser origin unavailable");
    let rpc_scheme = if location.protocol().as_deref() == Ok("https:") {
        "wss"
    } else {
        "ws"
    };
    let rpc_url = format!(
        "{rpc_scheme}://{}/rpc",
        location.host().expect("browser host unavailable")
    );

    let web_default_settings = web_default_settings();
    let settings_store = SettingsStore::new(cx, &web_default_settings);
    cx.set_global(settings_store);

    wasm_remote::set_remote_client(remote_client.clone());
    remote_client.on_notification("Browser::open_url", |params| {
        if let Some(url) = params.get("url").and_then(serde_json::Value::as_str) {
            open_external_url(url);
        }
    });
    smol::set_remote_client(remote_client.clone());
    terminal::set_remote_client(remote_client.clone());
    web_agent_panel::set_remote_client(remote_client.clone());
    // Server-side SQLite for workspace/KVP persistence.
    sqlez::remote_sql::set_sql_endpoint(format!("{server_origin}/sql"));
    sqlez::remote_sql::set_sql_rpc_endpoint(rpc_url.clone());
    sqlez::remote_sql::set_async_sql_client(remote_client.clone());

    let fs: Arc<dyn Fs> = Arc::new(RemoteFs::new(
        remote_client.clone(),
        cx.background_executor().clone(),
    ));
    <dyn Fs>::set_global(fs.clone(), cx);
    load_user_settings(fs.clone(), cx);

    let languages = Arc::new(language::LanguageRegistry::new(
        cx.background_executor().clone(),
    ));
    register_plain_languages(&languages);

    let clock: Arc<dyn clock::SystemClock> = Arc::new(RealSystemClock);
    // Model providers (Anthropic / OpenAI) call cx.http_client() directly.
    // Route their API hosts through the host server, which injects the key and
    // streams the upstream response back (avoids browser CORS + keeps keys on
    // the host). Other URLs go straight to browser fetch.
    // Safety: gpui_web marks `new` unsafe under its `multithreaded` feature
    // because fetch is only valid from the single browser main thread. The web
    // workspace runs the whole app on that one thread, so this is sound here.
    let fetch = unsafe { gpui_web::FetchHttpClient::new() };
    let proxy_http = Arc::new(web_proxy_http::ProxyHttpClient::new(
        Arc::new(fetch),
        &server_origin,
    ));
    cx.set_http_client(proxy_http.clone());
    let http_client = Arc::new(HttpClientWithUrl::new(
        Arc::new(BlockedHttpClient::new()),
        &server_origin,
        None,
    ));
    let client = Client::new(clock, http_client, cx);

    // Migrations and cache priming completed asynchronously before GPUI launch.
    let app_db = futures::FutureExt::now_or_never(db::AppDatabase::open_in_memory("zed-web"))
        .expect("in-memory AppDatabase should complete synchronously on wasm");
    cx.set_global(app_db);

    let session = cx.new(|cx| session::AppSession::new(session::Session::for_web(), cx));
    let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));

    let app_state = Arc::new(AppState {
        client: client.clone(),
        fs: fs.clone(),
        languages: languages.clone(),
        user_store,
        workspace_store,
        // Real Node.js runtime on the host: npm/node commands run server-side
        // via the remote process bridge. `detect()` (wasm path) uses bare
        // `node`/`npm` resolved against the server's PATH (nvm / homebrew).
        // allow_path_lookup=true lets it use the host's system Node.js.
        node_runtime: NodeRuntime::new(
            proxy_http.clone(),
            None,
            watch::channel(Some(node_runtime::NodeBinaryOptions {
                allow_path_lookup: true,
                allow_binary_download: false,
                use_paths: None,
            }))
            .1,
        ),
        build_window_options: web_window_options,
        session,
    });
    AppState::set_global(app_state.clone(), cx);

    // --- Desktop-parity inits (wasm-safe) ---
    menu::init();
    zed_actions::init();
    release_channel::init(semver::Version::new(0, 0, 0), cx);
    // Load bundled themes + icon themes from Assets (file tree icons, SVG UI icons).
    theme_settings::init(
        theme::LoadThemes::All(Box::new(WebAssets {
            extension_assets: extension_assets.clone(),
        })),
        cx,
    );
    if let Err(err) = assets::Assets.load_fonts(cx) {
        web_sys::console::error_1(&format!("zed_web_workspace: font load failed: {err:#}").into());
    }
    client::init(&client, cx);
    platform_title_bar::PlatformTitleBar::init(cx);

    command_palette::init(cx);
    language_model::init(cx);
    editor::init(cx);
    image_viewer::init(cx);
    markdown_preview::init(cx);
    csv_preview::init(cx);
    svg_preview::init(cx);
    diagnostics::init(cx);
    host_debug_adapter::init(cx);
    dap_adapters::init(cx);
    debugger_ui::init(cx);
    terminal_view::init(cx);
    workspace::init(app_state.clone(), cx);

    go_to_line::init(cx);
    file_finder::init(cx);
    tab_switcher::init(cx);
    outline::init(cx);
    project_symbols::init(cx);
    project_panel::init(cx);
    outline_panel::init(cx);
    search::init(cx);
    encoding_selector::init(cx);
    language_selector::init(cx);
    line_ending_selector::init(cx);
    toolchain_selector::init(cx);
    theme_selector::init(cx);
    tasks_ui::init(cx);
    snippet_provider::init(cx);
    snippets_ui::init(cx);
    which_key::init(cx);
    git_ui::init(cx);
    recent_projects::init(cx);
    // Real desktop Open/Save path picker (no OS file dialog in browser).
    cx.observe_new(open_path_prompt::OpenPathPrompt::register)
        .detach();
    cx.observe_new(open_path_prompt::OpenPathPrompt::register_new_path)
        .detach();
    install_web_open_actions(cx);

    // Agent: real desktop agent_ui + model providers + host-backed extensions/menu.
    {
        use settings::Settings as _;
        agent_settings::AgentSettings::register(cx);
    }
    // Cloud provider token refresh listener (desktop registers this before
    // language_models::init; CloudLanguageModelProvider::new reads its global).
    client::RefreshLlmTokenListener::register(
        app_state.client.clone(),
        app_state.user_store.clone(),
        cx,
    );
    // Model providers (Anthropic / OpenAI / …) talk over the wasm Fetch HTTP
    // client; API keys come from the host via the wasm credentials shim.
    language_models::init(app_state.user_store.clone(), app_state.client.clone(), cx);
    // Use the proxy-backed client (not the blocked `client.http_client()`) so the
    // ACP registry CDN fetch and other direct requests work in the browser.
    project::AgentRegistryStore::init_global(cx, app_state.fs.clone(), proxy_http.clone());
    let prompt_builder = prompt_store::PromptBuilder::load(app_state.fs.clone(), false, cx);
    agent_ui::init(
        app_state.fs.clone(),
        prompt_builder,
        app_state.languages.clone(),
        false, // is_new_install
        false, // is_eval
        cx,
    );
    // Panel-level agent actions (toggle/focus/toggle + inline assist). Desktop
    // registers these in zed.rs (not agent_ui::init), so the web shell must do
    // it explicitly — otherwise `agent::ToggleFocus` has no handler and the
    // panel can't be opened from the menu / keybinding.
    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        workspace
            .register_action(agent_ui::AgentPanel::toggle_focus)
            .register_action(agent_ui::AgentPanel::focus)
            .register_action(agent_ui::AgentPanel::toggle)
            .register_action(agent_ui::InlineAssistant::inline_assist);
    })
    .detach();
    // Real settings GUI window (audio / edit-prediction / MCP pages are stubbed
    // on wasm; everything else — General, Appearance, Editor, AI, Keymap, … — is
    // the desktop settings_ui). Registered before web_user_menu so its
    // OpenSettings/OpenSettingsPage handlers win over the JSON-file fallback.
    settings_ui::init(cx);
    keymap_editor::init(cx);
    // Present the settings GUI as an in-window modal popup (the web build has a
    // single canvas, so a separate OS settings window isn't possible).
    // settings_ui::init registers these as WORKSPACE actions, which outrank a
    // plain `cx.on_action`, so we must re-register at the workspace level (via
    // observe_new) AFTER settings_ui::init for our popup handler to win.
    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        log::info!("zed_web_workspace: registering settings popup actions on workspace");
        workspace
            .register_action(|workspace, _: &zed_actions::OpenSettings, window, cx| {
                web_settings_modal::open_settings_popup(None, None, workspace, window, cx);
            })
            .register_action(
                |workspace, action: &zed_actions::OpenSettingsPage, window, cx| {
                    web_settings_modal::open_settings_popup(
                        None,
                        Some(action.page.clone()),
                        workspace,
                        window,
                        cx,
                    );
                },
            )
            .register_action(|workspace, _: &zed_actions::OpenSettingsAt, window, cx| {
                web_settings_modal::open_settings_popup(None, None, workspace, window, cx);
            })
            .register_action(
                |workspace, _: &zed_actions::OpenProjectSettings, window, cx| {
                    web_settings_modal::open_settings_popup(None, None, workspace, window, cx);
                },
            )
            .register_action(|workspace, _: &zed_actions::OpenKeymap, window, cx| {
                web_settings_modal::open_settings_popup(
                    None,
                    Some("Keymap".to_string()),
                    workspace,
                    window,
                    cx,
                );
            });
    })
    .detach();
    web_agent_panel::init(cx);
    extensions_ui::init_remote_store(
        remote_client.clone(),
        languages.clone(),
        extension_assets,
        cx,
    );
    extensions_ui::init(cx);
    web_user_menu::init(cx);

    // Keymaps after actions are registered.
    load_keymaps(cx);
    watch_user_keymap(fs.clone(), cx);
    install_menus(cx);

    cx.set_global(workspace::PaneSearchBarCallbacks {
        setup_search_bar: |languages, toolbar, window, cx| {
            let search_bar = cx.new(|cx| search::BufferSearchBar::new(languages, window, cx));
            toolbar.update(cx, |toolbar, cx| {
                toolbar.add_item(search_bar, window, cx);
            });
        },
        wrap_div_with_search_actions: search::buffer_search::register_pane_search_actions,
    });

    languages.set_theme(cx.theme().clone());
    let languages_for_theme = languages.clone();
    cx.observe_global::<theme::GlobalTheme>(move |cx| {
        languages_for_theme.set_theme(cx.theme().clone());
    })
    .detach();

    install_workspace_chrome(cx);

    // Syntax highlighting runs on the server (Pygments now; tree-sitter/LSP later).
    remote_highlight::install(remote_client, cx);

    web_sys::console::log_1(&"zed_web_workspace: desktop-style subsystem init complete".into());

    app_state
}

#[cfg(target_family = "wasm")]
fn install_workspace_chrome(cx: &mut App) {
    use workspace::Workspace;

    cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        // Title bar + in-window application menu (desktop Linux/Windows style)
        let multi_workspace = workspace.multi_workspace().cloned();
        let project = workspace.project().clone();
        sync_project_paths_url(&project, cx);
        cx.subscribe_in(&project, window, |_, project, event, _, cx| {
            if matches!(
                event,
                project::Event::WorktreeAdded(_)
                    | project::Event::WorktreeRemoved(_)
                    | project::Event::WorktreeOrderChanged
                    | project::Event::WorktreePathsChanged { .. }
            ) {
                sync_project_paths_url(project, cx);
            }
        })
        .detach();
        let title_bar = cx.new(|cx| {
            WebTitleBar::new(
                workspace.weak_handle(),
                multi_workspace.clone(),
                project,
                window,
                cx,
            )
        });
        workspace.set_titlebar_item(title_bar.into(), window, cx);

        // Real desktop threads sidebar (the full `sidebar` crate — same UI as
        // desktop: project groups, filter, archive, thread switcher). Replaces
        // the earlier minimal custom WebSidebar.
        if let Some(multi_workspace) = multi_workspace.and_then(|mw| mw.upgrade()) {
            cx.subscribe_in(
                &multi_workspace,
                window,
                |_, multi_workspace, event: &workspace::MultiWorkspaceEvent, _, cx| {
                    if matches!(
                        event,
                        workspace::MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                            | workspace::MultiWorkspaceEvent::WorkspaceAdded(_)
                            | workspace::MultiWorkspaceEvent::WorkspaceRemoved(_)
                            | workspace::MultiWorkspaceEvent::ProjectGroupsChanged
                    ) {
                        let multi_workspace = multi_workspace.downgrade();
                        cx.defer(move |cx| {
                            if let Some(multi_workspace) = multi_workspace.upgrade() {
                                let groups =
                                    multi_workspace_project_groups(multi_workspace.read(cx), cx);
                                sync_project_groups_url(&groups);
                            }
                        });
                    }
                },
            )
            .detach();
            let sidebar = cx.new(|cx| sidebar::Sidebar::new(multi_workspace.clone(), window, cx));
            multi_workspace.update(cx, |mw, cx| {
                mw.register_sidebar(sidebar, cx);
            });
        }

        // Pane chrome
        let workspace_handle = cx.entity();
        let center_pane = workspace.active_pane().clone();
        initialize_pane_chrome(workspace, &center_pane, window, cx);
        cx.subscribe_in(&workspace_handle, window, {
            move |workspace, _, event, window, cx| {
                if let workspace::Event::PaneAdded(pane) = event {
                    initialize_pane_chrome(workspace, pane, window, cx);
                }
            }
        })
        .detach();

        // Status bar — mirrors desktop zed's `initialize_workspace` status items.
        let search_button = cx.new(|_| search::search_status_button::SearchButton::new());
        let diagnostic_summary =
            cx.new(|cx| diagnostics::items::DiagnosticIndicator::new(workspace, cx));
        let active_file_name = cx.new(|_| workspace::active_file_name::ActiveFileName::new());
        let active_buffer_encoding =
            cx.new(|_| encoding_selector::ActiveBufferEncoding::new(workspace));
        let active_buffer_language =
            cx.new(|_| language_selector::ActiveBufferLanguage::new(workspace));
        let active_toolchain =
            cx.new(|cx| toolchain_selector::ActiveToolchain::new(workspace, window, cx));
        let cursor_position =
            cx.new(|_| go_to_line::cursor_position::CursorPosition::new(workspace));
        let line_ending_indicator =
            cx.new(|_| line_ending_selector::LineEndingIndicator::default());
        let git_blame_status = cx.new(|_| git_ui::GitBlameStatus::default());
        let merge_conflict_indicator =
            cx.new(|cx| git_ui::MergeConflictIndicator::new(workspace, cx));
        let activity_indicator = activity_indicator::ActivityIndicator::new(
            workspace,
            workspace.project().read(cx).languages().clone(),
            window,
            cx,
        );
        let edit_prediction_menu_handle = ui::PopoverMenuHandle::default();
        let edit_prediction_button = cx.new(|cx| {
            edit_prediction_ui::EditPredictionButton::new(
                workspace.app_state().fs.clone(),
                workspace.app_state().user_store.clone(),
                edit_prediction_menu_handle.clone(),
                workspace.project().clone(),
                cx,
            )
        });
        workspace.register_action({
            move |_, _: &edit_prediction_ui::ToggleMenu, window, cx| {
                edit_prediction_menu_handle.toggle(window, cx);
            }
        });
        let lsp_button_menu_handle = ui::PopoverMenuHandle::default();
        let lsp_button = cx.new(|cx| {
            language_tools::lsp_button::LspButton::new(
                workspace,
                lsp_button_menu_handle.clone(),
                window,
                cx,
            )
        });
        workspace.register_action({
            move |_, _: &language_tools::lsp_button::ToggleMenu, window, cx| {
                lsp_button_menu_handle.toggle(window, cx);
            }
        });

        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(search_button, window, cx);
            status_bar.add_left_item(lsp_button, window, cx);
            status_bar.add_left_item(diagnostic_summary, window, cx);
            status_bar.add_left_item(active_file_name, window, cx);
            status_bar.add_left_item(git_blame_status, window, cx);
            status_bar.add_left_item(merge_conflict_indicator, window, cx);
            status_bar.add_left_item(activity_indicator, window, cx);
            status_bar.add_right_item(edit_prediction_button, window, cx);
            status_bar.add_right_item(active_buffer_encoding, window, cx);
            status_bar.add_right_item(active_buffer_language, window, cx);
            status_bar.add_right_item(active_toolchain, window, cx);
            status_bar.add_right_item(line_ending_indicator, window, cx);
            status_bar.add_right_item(cursor_position, window, cx);
        });

        let panels_started = Arc::new(AtomicBool::new(false));
        let workspace_handle = cx.entity();
        let panels_started_on_restore = panels_started.clone();
        cx.observe_in(
            &workspace_handle,
            window,
            move |workspace, _, window, cx| {
                if workspace.initial_state_loaded()
                    && !panels_started_on_restore.swap(true, Ordering::AcqRel)
                {
                    let panels_task = load_core_panels(window, cx);
                    workspace.set_panels_task(panels_task);
                }
            },
        )
        .detach();

        if workspace.initial_state_loaded() && !panels_started.swap(true, Ordering::AcqRel) {
            let panels_task = load_core_panels(window, cx);
            workspace.set_panels_task(panels_task);
        }

        if !workspace.has_active_modal(window, cx) {
            workspace.focus_handle(cx).focus(window, cx);
        }
    })
    .detach();
}

#[cfg(target_family = "wasm")]
struct WebTitleBar {
    platform: Entity<platform_title_bar::PlatformTitleBar>,
    project: Entity<project::Project>,
    menu_bar: Entity<web_menu_bar::WebMenuBar>,
    multi_workspace: Option<WeakEntity<workspace::MultiWorkspace>>,
    workspace: WeakEntity<workspace::Workspace>,
    _subscriptions: Vec<Subscription>,
}

#[cfg(target_family = "wasm")]
impl WebTitleBar {
    fn new(
        workspace: WeakEntity<workspace::Workspace>,
        multi_workspace: Option<WeakEntity<workspace::MultiWorkspace>>,
        project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let platform = cx.new(|cx| {
            let mut bar = platform_title_bar::PlatformTitleBar::new("zed-web-title-bar", cx);
            if let Some(mw) = multi_workspace.clone() {
                bar = bar.with_multi_workspace(mw);
            }
            bar
        });
        // Build after cx.set_menus so get_menus() is populated.
        let menu_bar = cx.new(|cx| web_menu_bar::WebMenuBar::new(window, cx));
        let mut subscriptions = Vec::new();
        if let Some(multi_workspace) = multi_workspace.as_ref().and_then(WeakEntity::upgrade) {
            subscriptions.push(cx.observe(&multi_workspace, |_, _, cx| cx.notify()));
        }
        Self {
            platform,
            project,
            menu_bar,
            multi_workspace,
            workspace,
            _subscriptions: subscriptions,
        }
    }
}

#[cfg(target_family = "wasm")]
impl gpui::Render for WebTitleBar {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use ui::{PopoverMenu, prelude::*};

        let active_workspace = self
            .multi_workspace
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
            .or_else(|| self.workspace.upgrade());
        let active_project = active_workspace
            .as_ref()
            .map(|workspace| workspace.read(cx).project().clone())
            .unwrap_or_else(|| self.project.clone());
        let project = active_project.read(cx);
        let worktrees: Vec<_> = project.visible_worktrees(cx).collect();
        let path_hint = worktrees
            .first()
            .map(|wt| wt.read(cx).abs_path().display().to_string())
            .unwrap_or_default();
        let display_name = worktrees
            .first()
            .and_then(|wt| {
                let path = wt.read(cx).abs_path();
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Zed Remote".to_string());
        let focus_handle = active_workspace
            .as_ref()
            .map(|workspace| workspace.read(cx).focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle());
        let window_project_groups = self
            .multi_workspace
            .as_ref()
            .and_then(|multi_workspace| multi_workspace.upgrade())
            .map(|multi_workspace| multi_workspace_project_groups(multi_workspace.read(cx), cx))
            .unwrap_or_default();
        sync_project_groups_url(&window_project_groups);
        let workspace = active_workspace
            .as_ref()
            .map(Entity::downgrade)
            .unwrap_or_else(|| self.workspace.clone());
        let popover_workspace = workspace.clone();
        let project_switcher = PopoverMenu::new("recent-projects-menu")
            .menu(move |window, cx| {
                Some(recent_projects::RecentProjects::popover(
                    popover_workspace.clone(),
                    window_project_groups.clone(),
                    Some(false),
                    focus_handle.clone(),
                    window,
                    cx,
                ))
            })
            .trigger(
                Button::new("project_name_trigger", display_name)
                    .label_size(LabelSize::Small)
                    .tab_index(0isize)
                    .when(worktrees.len() > 1, |button| {
                        button.end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            );
        // Layout matches desktop Linux/Windows title bar:
        // [ File Edit View … ] [▾ project] path          Zed Web  [▾ user menu]
        let user_menu = web_user_menu::render_user_menu_button(workspace, cx);
        let children: Vec<gpui::AnyElement> = vec![
            h_flex()
                .gap_2()
                .items_center()
                .h_full()
                .child(self.menu_bar.clone())
                .child(div().mx_1().w(px(1.)).h_3().bg(cx.theme().colors().border))
                .child(project_switcher)
                .child(
                    Label::new(if path_hint.is_empty() {
                        "remote".to_string()
                    } else {
                        path_hint
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .into_any_element(),
            div().flex_1().into_any_element(),
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new("Zed Web")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(user_menu)
                .into_any_element(),
        ];

        self.platform.update(cx, |bar, _cx| {
            bar.set_children(children);
        });

        self.platform.clone().into_any_element()
    }
}

#[cfg(target_family = "wasm")]
fn initialize_pane_chrome(
    workspace: &mut workspace::Workspace,
    pane: &Entity<workspace::Pane>,
    window: &mut Window,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    // Pane toolbar — mirrors desktop zed's `initialize_pane`: breadcrumbs +
    // buffer search bar + quick action bar (preview / search / inline assist /
    // selections / editor settings buttons under the file tab).
    let buffer_search_bar = cx.new(|cx| {
        search::BufferSearchBar::new(
            Some(workspace.project().read(cx).languages().clone()),
            window,
            cx,
        )
    });
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let breadcrumbs = cx.new(|_| breadcrumbs::Breadcrumbs::new());
            toolbar.add_item(breadcrumbs, window, cx);
            toolbar.add_item(buffer_search_bar.clone(), window, cx);
            let quick_action_bar = cx.new(|cx| {
                web_quick_action_bar::WebQuickActionBar::new(
                    buffer_search_bar.clone(),
                    workspace,
                    cx,
                )
            });
            toolbar.add_item(quick_action_bar, window, cx);
            let project_search_bar = cx.new(|_| search::project_search::ProjectSearchBar::new());
            toolbar.add_item(project_search_bar, window, cx);
        });
    });

    // The pane's BufferSearchBar is created + added above (shared with the quick
    // action bar). The PaneSearchBarCallbacks global is only kept for panes that
    // request it dynamically; do NOT add a second search bar here.
}

#[cfg(target_family = "wasm")]
fn load_core_panels(
    window: &mut Window,
    cx: &mut gpui::Context<workspace::Workspace>,
) -> gpui::Task<anyhow::Result<()>> {
    use agent_ui::AgentPanel;
    use debugger_ui::debugger_panel::DebugPanel;
    use futures::Future;
    use git_ui::git_panel::GitPanel;
    use outline_panel::OutlinePanel;
    use project_panel::ProjectPanel;
    use terminal_view::terminal_panel::TerminalPanel;

    cx.spawn_in(window, async move |workspace_handle, cx| {
        async fn add_panel_when_ready(
            panel_task: impl Future<Output = anyhow::Result<Entity<impl workspace::Panel>>> + 'static,
            workspace_handle: WeakEntity<workspace::Workspace>,
            mut cx: gpui::AsyncWindowContext,
            name: &'static str,
        ) -> bool {
            match panel_task.await {
                Ok(panel) => {
                    let attached = workspace_handle
                        .update_in(&mut cx, |workspace, window, cx| {
                            workspace.add_panel(panel, window, cx);
                        })
                        .is_ok();
                    web_sys::console::log_1(
                        &format!("zed_web_workspace: {name} panel attached").into(),
                    );
                    attached
                }
                Err(err) => {
                    web_sys::console::error_1(
                        &format!("zed_web_workspace: {name} panel failed: {err:#}").into(),
                    );
                    false
                }
            }
        }

        let project_panel = ProjectPanel::load(workspace_handle.clone(), cx.clone());
        let outline_panel = OutlinePanel::load(workspace_handle.clone(), cx.clone());
        let git_panel = GitPanel::load(workspace_handle.clone(), cx.clone());
        let mut debug_cx = cx.clone();
        let debug_panel = DebugPanel::load(workspace_handle.clone(), &mut debug_cx);
        // Real desktop TerminalPanel: PTY I/O is RemotePty → server Terminal::*.
        let terminal_panel = TerminalPanel::load(workspace_handle.clone(), cx.clone());
        // Real desktop AgentPanel: native agent runs in-process over remote Fs +
        // remote SQL; model providers stream over the wasm Fetch HTTP client.
        let agent_panel = AgentPanel::load(workspace_handle.clone(), cx.clone());

        let attached = futures::join!(
            add_panel_when_ready(
                project_panel,
                workspace_handle.clone(),
                cx.clone(),
                "project"
            ),
            add_panel_when_ready(
                outline_panel,
                workspace_handle.clone(),
                cx.clone(),
                "outline"
            ),
            add_panel_when_ready(git_panel, workspace_handle.clone(), cx.clone(), "git"),
            add_panel_when_ready(
                debug_panel,
                workspace_handle.clone(),
                cx.clone(),
                "debugger"
            ),
            add_panel_when_ready(
                terminal_panel,
                workspace_handle.clone(),
                cx.clone(),
                "terminal"
            ),
            add_panel_when_ready(
                agent_panel,
                workspace_handle.clone(),
                cx.clone(),
                "agent"
            ),
        );

        workspace_handle
            .update_in(cx, |workspace, window, cx| {
                if saved_agent_panel_open() {
                    workspace.open_panel::<AgentPanel>(window, cx);
                }

                let sync_visibility = |workspace: &workspace::Workspace, cx: &App| {
                    let panel_id = workspace
                        .panel::<AgentPanel>(cx)
                        .map(|panel| panel.entity_id());
                    let visible = panel_id.is_some_and(|panel_id| {
                        workspace.all_docks().iter().any(|dock| {
                            dock.read(cx)
                                .visible_panel()
                                .is_some_and(|panel| panel.panel_id() == panel_id)
                        })
                    });
                    save_agent_panel_open(visible);
                };
                sync_visibility(workspace, cx);

                for dock in workspace.all_docks() {
                    cx.observe(dock, move |workspace, _, cx| {
                        sync_visibility(workspace, cx);
                    })
                    .detach();
                }
            })
            .ok();

        if [
            attached.0, attached.1, attached.2, attached.3, attached.4, attached.5,
        ]
        .into_iter()
        .all(|attached| attached)
        {
            web_sys::console::log_1(
                &"zed_web_workspace: docks ready (saved workspace layout restored)".into(),
            );
        } else {
            web_sys::console::warn_1(
                &"zed_web_workspace: incomplete panel load; preserving prior layout".into(),
            );
        }
        Ok(())
    })
}

#[cfg(target_family = "wasm")]
fn log_open_result(
    open_result_task: gpui::Task<anyhow::Result<workspace::OpenResult>>,
    project_groups: Vec<Vec<std::path::PathBuf>>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        web_sys::console::log_1(&"zed_web_workspace: awaiting open_paths…".into());
        match open_result_task.await {
            Ok(result) => {
                let restore_sidebar = saved_workspace_sidebar_open();
                if !project_groups.is_empty() {
                    let groups = project_groups
                        .into_iter()
                        .map(|paths| workspace::SerializedProjectGroupState {
                            key: project::ProjectGroupKey::new(
                                None,
                                workspace::PathList::new(&paths),
                            ),
                            expanded: true,
                        })
                        .collect();
                    result
                        .window
                        .update(cx, |multi_workspace, _, cx| {
                            multi_workspace.restore_project_groups(groups, cx);
                            multi_workspace.serialize(cx);
                        })
                        .log_err();
                }
                let window = result.window;
                cx.spawn(async move |cx| {
                    let mut restore_pending = restore_sidebar;
                    loop {
                        let Some(open) = window
                            .update(cx, |multi_workspace, _, cx| {
                                if restore_pending && !multi_workspace.sidebar_open() {
                                    multi_workspace.open_sidebar(cx);
                                }
                                multi_workspace.sidebar_open()
                            })
                            .ok()
                        else {
                            break;
                        };
                        if open {
                            restore_pending = false;
                        }
                        if !restore_pending {
                            save_workspace_sidebar_open(open);
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(500))
                            .await;
                    }
                })
                .detach();
                web_sys::console::log_1(&"zed_web_workspace: open_paths succeeded".into());
            }
            Err(err) => web_sys::console::error_1(
                &format!("zed_web_workspace: open_paths failed: {err:#}").into(),
            ),
        }
        Some(())
    })
    .detach();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen(start)]
pub fn main() {
    // The wasm-bindgen thread transform disrupts the automatic
    // `__wasm_call_ctors` invocation that `inventory`/`ctor` static
    // initializers (settings registration, sqlez migrations) rely on. TLS is
    // initialized by the time `main` runs (inside `__wbindgen_start`), so it's
    // now safe to run the static constructors before anything reads them.
    // The glue exposes `__wasm_call_ctors` as `window.__zedCallCtors`.
    if let Some(window) = web_sys::window() {
        use wasm_bindgen::JsCast as _;
        let ctor =
            js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("__zedCallCtors"))
                .ok()
                .and_then(|v| v.dyn_into::<js_sys::Function>().ok());
        if let Some(f) = ctor {
            let _ = f.call0(&wasm_bindgen::JsValue::UNDEFINED);
        }
    }

    wasm_bindgen_futures::spawn_local(async move {
        let location = web_sys::window()
            .expect("browser window unavailable")
            .location();
        let server_origin = location.origin().expect("browser origin unavailable");
        let rpc_scheme = if location.protocol().as_deref() == Ok("https:") {
            "wss"
        } else {
            "ws"
        };
        let rpc_url = format!(
            "{rpc_scheme}://{}/rpc",
            location.host().expect("browser host unavailable")
        );
        let remote_client =
            wasm_remote::RemoteClient::connect(&rpc_url).expect("WebSocket connect failed");
        sqlez::remote_sql::set_sql_endpoint(format!("{server_origin}/sql"));
        sqlez::remote_sql::set_sql_rpc_endpoint(rpc_url);
        sqlez::remote_sql::set_async_sql_client(remote_client.clone());
        let initialization = futures::try_join!(load_web_assets(), db::prepare_web_database());
        if let Err(error) = initialization {
            web_sys::console::error_1(
                &format!("zed_web_workspace: initialization failed: {error:#}").into(),
            );
            return;
        }
        launch(remote_client);
    });
}

#[cfg(target_family = "wasm")]
fn launch(remote_client: wasm_remote::RemoteClient) {
    use std::path::PathBuf;

    gpui_platform::web_init();
    // Bundle fonts/icons/themes so SVG icons and keymaps assets resolve.
    let extension_assets = Arc::new(RwLock::new(BTreeMap::new()));
    let web_assets = WebAssets {
        extension_assets: extension_assets.clone(),
    };
    let handle = gpui_platform::single_threaded_web()
        .with_assets(web_assets)
        .run_embedded(move |cx: &mut App| {
            let app_state = init_app_state(cx, extension_assets.clone(), remote_client.clone());

            let mut paths = workspace_paths_from_url();
            if paths.is_empty() {
                paths.push(PathBuf::from(WORKSPACE_ROOT));
            }
            let project_groups = workspace_project_groups_from_url();
            let open_task =
                workspace::open_paths(&paths, app_state, workspace::OpenOptions::default(), cx);
            log_open_result(open_task, project_groups, cx);
            cx.activate(true);
        });
    std::mem::forget(handle);
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    println!("This binary is only meant to run as WASM in a browser.");
}
