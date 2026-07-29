use std::{
    io::{BufRead as _, Write as _},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use dap::adapters::{DebugAdapterName, DebugTaskDefinition};
use extension::{
    CodeLabelSpan, Completion, CompletionKind, CompletionLabelDetails, Extension as _,
    ExtensionHostProxy, ExtensionManifest, InsertTextFormat, KeyValueStoreDelegate,
    ProjectDelegate, SlashCommand, Symbol, SymbolKind, WorktreeDelegate,
};
use extension_host::wasm_host::{WasmExtension, WasmHost};
use fs::{Fs, RealFs};
use language::LanguageName;
use lsp::LanguageServerName;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use reqwest_client::ReqwestClient;
use serde::Deserialize;
use serde_json::{Value, json};
use util::rel_path::RelPath;

const USER_AGENT: &str = "ZedWeb-Extension-Runtime/1.0";
const RESPONSE_PREFIX: &str = "ZED_EXTENSION_RESPONSE ";

#[derive(Deserialize)]
struct Request {
    method: String,
    extension_dir: PathBuf,
    worktree_root: PathBuf,
    worktree_id: Option<u64>,
    worktree_env: Option<collections::HashMap<String, String>>,
    language_server_id: Option<String>,
    target_language_server_id: Option<String>,
    language_name: Option<String>,
    debug_adapter_name: Option<String>,
    label: Option<String>,
    config: Option<Value>,
    tcp_connection: Option<task::TcpArgumentsTemplate>,
    user_installed_path: Option<PathBuf>,
    zed_scenario: Option<Value>,
    debug_locator_name: Option<String>,
    build_config: Option<Value>,
    resolved_label: Option<String>,
    adapter_name: Option<String>,
    completions: Option<Vec<CompletionRequest>>,
    symbols: Option<Vec<SymbolRequest>>,
    slash_command: Option<SlashCommandRequest>,
    arguments: Option<Vec<String>>,
    context_server_id: Option<String>,
    worktree_ids: Option<Vec<u64>>,
    provider: Option<String>,
    package_name: Option<String>,
}

#[derive(Deserialize)]
struct CompletionRequest {
    label: String,
    label_details: Option<CompletionLabelDetailsRequest>,
    detail: Option<String>,
    kind: Option<i32>,
    insert_text_format: Option<i32>,
}

#[derive(Deserialize)]
struct CompletionLabelDetailsRequest {
    detail: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct SymbolRequest {
    kind: i32,
    name: String,
    container_name: Option<String>,
}

#[derive(Deserialize)]
struct SlashCommandRequest {
    name: String,
    description: String,
    tooltip_text: String,
    requires_argument: bool,
}

#[derive(Deserialize)]
struct ResolvedBuildTaskRequest {
    label: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: collections::HashMap<String, String>,
    cwd: Option<PathBuf>,
}

struct LocalWorktree {
    id: u64,
    root: PathBuf,
    env: collections::HashMap<String, String>,
}

struct LocalProject {
    worktree_ids: Vec<u64>,
}

impl ProjectDelegate for LocalProject {
    fn worktree_ids(&self) -> Vec<u64> {
        self.worktree_ids.clone()
    }
}

#[derive(Default)]
struct LocalKeyValueStore {
    values: Mutex<collections::HashMap<String, String>>,
}

impl KeyValueStoreDelegate for LocalKeyValueStore {
    fn insert(&self, key: String, docs: String) -> gpui::Task<Result<()>> {
        self.values.lock().unwrap().insert(key, docs);
        gpui::Task::ready(Ok(()))
    }
}

#[async_trait]
impl WorktreeDelegate for LocalWorktree {
    fn id(&self) -> u64 {
        self.id
    }

    fn root_path(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    async fn read_text_file(&self, path: &RelPath) -> Result<String> {
        Ok(smol::fs::read_to_string(self.root.join(path.as_std_path())).await?)
    }

    async fn which(&self, binary_name: String) -> Option<String> {
        let path = self.env.get("PATH").map(std::ffi::OsStr::new);
        which::which_in(binary_name, path, &self.root)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }

    async fn shell_env(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

fn completion_kind(kind: i32) -> CompletionKind {
    match kind {
        1 => CompletionKind::Text,
        2 => CompletionKind::Method,
        3 => CompletionKind::Function,
        4 => CompletionKind::Constructor,
        5 => CompletionKind::Field,
        6 => CompletionKind::Variable,
        7 => CompletionKind::Class,
        8 => CompletionKind::Interface,
        9 => CompletionKind::Module,
        10 => CompletionKind::Property,
        11 => CompletionKind::Unit,
        12 => CompletionKind::Value,
        13 => CompletionKind::Enum,
        14 => CompletionKind::Keyword,
        15 => CompletionKind::Snippet,
        16 => CompletionKind::Color,
        17 => CompletionKind::File,
        18 => CompletionKind::Reference,
        19 => CompletionKind::Folder,
        20 => CompletionKind::EnumMember,
        21 => CompletionKind::Constant,
        22 => CompletionKind::Struct,
        23 => CompletionKind::Event,
        24 => CompletionKind::Operator,
        25 => CompletionKind::TypeParameter,
        other => CompletionKind::Other(other),
    }
}

fn symbol_kind(kind: i32) -> SymbolKind {
    match kind {
        1 => SymbolKind::File,
        2 => SymbolKind::Module,
        3 => SymbolKind::Namespace,
        4 => SymbolKind::Package,
        5 => SymbolKind::Class,
        6 => SymbolKind::Method,
        7 => SymbolKind::Property,
        8 => SymbolKind::Field,
        9 => SymbolKind::Constructor,
        10 => SymbolKind::Enum,
        11 => SymbolKind::Interface,
        12 => SymbolKind::Function,
        13 => SymbolKind::Variable,
        14 => SymbolKind::Constant,
        15 => SymbolKind::String,
        16 => SymbolKind::Number,
        17 => SymbolKind::Boolean,
        18 => SymbolKind::Array,
        19 => SymbolKind::Object,
        20 => SymbolKind::Key,
        21 => SymbolKind::Null,
        22 => SymbolKind::EnumMember,
        23 => SymbolKind::Struct,
        24 => SymbolKind::Event,
        25 => SymbolKind::Operator,
        26 => SymbolKind::TypeParameter,
        other => SymbolKind::Other(other),
    }
}

fn code_label_json(label: extension::CodeLabel) -> Value {
    let spans = label
        .spans
        .into_iter()
        .map(|span| match span {
            CodeLabelSpan::CodeRange(range) => {
                json!({"kind": "code", "start": range.start, "end": range.end})
            }
            CodeLabelSpan::Literal(literal) => json!({
                "kind": "literal",
                "text": literal.text,
                "highlight_name": literal.highlight_name,
            }),
        })
        .collect::<Vec<_>>();
    json!({
        "code": label.code,
        "spans": spans,
        "filter_start": label.filter_range.start,
        "filter_end": label.filter_range.end,
    })
}

async fn load_extension(request: &Request, cx: &mut gpui::AsyncApp) -> Result<WasmExtension> {
    let extension_dir = request
        .extension_dir
        .canonicalize()
        .context("invalid extension directory")?;
    let manifest = Arc::new(
        ExtensionManifest::load(
            Arc::new(RealFs::new(None, cx.background_executor().clone())),
            &extension_dir,
        )
        .await?,
    );
    let http = Arc::new(ReqwestClient::user_agent(USER_AGENT)?);
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    let node_runtime = NodeRuntime::new(
        http.clone(),
        None,
        watch::channel(Some(NodeBinaryOptions {
            allow_path_lookup: true,
            allow_binary_download: true,
            use_paths: None,
        }))
        .1,
    );
    let wasm_host = cx.update(|cx| {
        WasmHost::new(
            fs,
            http,
            node_runtime,
            Arc::new(ExtensionHostProxy::new()),
            extension_dir.join(".zed-runtime"),
            cx,
        )
    });
    WasmExtension::load(&extension_dir, &manifest, wasm_host, cx).await
}

async fn execute(extension: &WasmExtension, request: Request) -> Result<Value> {
    let worktree: Arc<dyn WorktreeDelegate> = Arc::new(LocalWorktree {
        id: request.worktree_id.unwrap_or(1),
        root: request.worktree_root,
        env: request
            .worktree_env
            .unwrap_or_else(|| std::env::vars().collect()),
    });
    let server_id = request
        .language_server_id
        .map(|name| LanguageServerName(name.into()));

    match request.method.as_str() {
        "language_server_command" => {
            let server_id = server_id.context("missing language_server_id")?;
            let language = request
                .language_name
                .map(|name| LanguageName::new(&name))
                .context("missing language_name")?;
            let command = extension
                .language_server_command(server_id, language, worktree)
                .await?;
            let command_path = extension.path_from_extension(&command.command);
            Ok(json!({
                "command": command_path,
                "args": command.args,
                "env": command.env,
            }))
        }
        "language_server_initialization_options" => {
            let server_id = server_id.context("missing language_server_id")?;
            let language = request
                .language_name
                .map(|name| LanguageName::new(&name))
                .context("missing language_name")?;
            let value = extension
                .language_server_initialization_options(server_id, language, worktree)
                .await?;
            Ok(json!(value))
        }
        "language_server_workspace_configuration" => {
            let server_id = server_id.context("missing language_server_id")?;
            Ok(json!(
                extension
                    .language_server_workspace_configuration(server_id, worktree)
                    .await?
            ))
        }
        "language_server_initialization_options_schema" => {
            let server_id = server_id.context("missing language_server_id")?;
            Ok(json!(
                extension
                    .language_server_initialization_options_schema(server_id, worktree)
                    .await?
            ))
        }
        "language_server_workspace_configuration_schema" => {
            let server_id = server_id.context("missing language_server_id")?;
            Ok(json!(
                extension
                    .language_server_workspace_configuration_schema(server_id, worktree)
                    .await?
            ))
        }
        "language_server_additional_initialization_options"
        | "language_server_additional_workspace_configuration" => {
            let server_id = server_id.context("missing language_server_id")?;
            let target = request
                .target_language_server_id
                .map(|name| LanguageServerName(name.into()))
                .context("missing target_language_server_id")?;
            let value = if request.method.ends_with("initialization_options") {
                extension
                    .language_server_additional_initialization_options(server_id, target, worktree)
                    .await?
            } else {
                extension
                    .language_server_additional_workspace_configuration(server_id, target, worktree)
                    .await?
            };
            Ok(json!(value))
        }
        "labels_for_completions" => {
            let server_id = server_id.context("missing language_server_id")?;
            let completions = request
                .completions
                .context("missing completions")?
                .into_iter()
                .map(|completion| Completion {
                    label: completion.label,
                    label_details: completion
                        .label_details
                        .map(|details| CompletionLabelDetails {
                            detail: details.detail,
                            description: details.description,
                        }),
                    detail: completion.detail,
                    kind: completion.kind.map(completion_kind),
                    insert_text_format: completion.insert_text_format.map(|format| match format {
                        1 => InsertTextFormat::PlainText,
                        2 => InsertTextFormat::Snippet,
                        other => InsertTextFormat::Other(other),
                    }),
                })
                .collect();
            let labels = extension
                .labels_for_completions(server_id, completions)
                .await?
                .into_iter()
                .map(|label| label.map(code_label_json))
                .collect::<Vec<_>>();
            Ok(json!(labels))
        }
        "labels_for_symbols" => {
            let server_id = server_id.context("missing language_server_id")?;
            let symbols = request
                .symbols
                .context("missing symbols")?
                .into_iter()
                .map(|symbol| Symbol {
                    kind: symbol_kind(symbol.kind),
                    name: symbol.name,
                    container_name: symbol.container_name,
                })
                .collect();
            let labels = extension
                .labels_for_symbols(server_id, symbols)
                .await?
                .into_iter()
                .map(|label| label.map(code_label_json))
                .collect::<Vec<_>>();
            Ok(json!(labels))
        }
        "complete_slash_command_argument" | "run_slash_command" => {
            let command =
                request
                    .slash_command
                    .context("missing slash_command")
                    .map(|command| SlashCommand {
                        name: command.name,
                        description: command.description,
                        tooltip_text: command.tooltip_text,
                        requires_argument: command.requires_argument,
                    })?;
            let arguments = request.arguments.unwrap_or_default();
            if request.method == "complete_slash_command_argument" {
                let completions = extension
                    .complete_slash_command_argument(command, arguments)
                    .await?
                    .into_iter()
                    .map(|completion| {
                        json!({
                            "label": completion.label,
                            "new_text": completion.new_text,
                            "run_command": completion.run_command,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json!(completions))
            } else {
                let output = extension
                    .run_slash_command(command, arguments, Some(worktree))
                    .await?;
                Ok(json!({
                    "text": output.text,
                    "sections": output.sections.into_iter().map(|section| json!({
                        "start": section.range.start,
                        "end": section.range.end,
                        "label": section.label,
                    })).collect::<Vec<_>>(),
                }))
            }
        }
        "context_server_command" | "context_server_configuration" => {
            let context_server_id: Arc<str> = request
                .context_server_id
                .context("missing context_server_id")?
                .into();
            let project: Arc<dyn ProjectDelegate> = Arc::new(LocalProject {
                worktree_ids: request.worktree_ids.unwrap_or_default(),
            });
            if request.method == "context_server_command" {
                let command = extension
                    .context_server_command(context_server_id, project)
                    .await?;
                Ok(json!({
                    "command": extension.path_from_extension(&command.command),
                    "args": command.args,
                    "env": command.env,
                }))
            } else {
                let configuration = extension
                    .context_server_configuration(context_server_id, project)
                    .await?;
                Ok(match configuration {
                    Some(configuration) => json!({
                        "installation_instructions": configuration.installation_instructions,
                        "settings_schema": configuration.settings_schema,
                        "default_settings": configuration.default_settings,
                    }),
                    None => Value::Null,
                })
            }
        }
        "suggest_docs_packages" => Ok(json!(
            extension
                .suggest_docs_packages(request.provider.context("missing provider")?.into(),)
                .await?
        )),
        "index_docs" => {
            let store = Arc::new(LocalKeyValueStore::default());
            extension
                .index_docs(
                    request.provider.context("missing provider")?.into(),
                    request.package_name.context("missing package_name")?.into(),
                    store.clone(),
                )
                .await?;
            let values = store.values.lock().unwrap().clone();
            Ok(json!(values))
        }
        "get_dap_binary" => {
            let adapter_name: Arc<str> = request
                .debug_adapter_name
                .context("missing debug_adapter_name")?
                .into();
            let definition = DebugTaskDefinition {
                label: request
                    .label
                    .unwrap_or_else(|| adapter_name.to_string())
                    .into(),
                adapter: DebugAdapterName(adapter_name.clone().into()),
                config: request.config.context("missing config")?,
                tcp_connection: request.tcp_connection,
            };
            let binary = extension
                .get_dap_binary(
                    adapter_name,
                    definition,
                    request.user_installed_path,
                    worktree,
                )
                .await?;
            Ok(json!(binary))
        }
        "dap_request_kind" => {
            let adapter_name: Arc<str> = request
                .debug_adapter_name
                .context("missing debug_adapter_name")?
                .into();
            Ok(json!(
                extension
                    .dap_request_kind(adapter_name, request.config.context("missing config")?,)
                    .await?
            ))
        }
        "dap_config_to_scenario" => {
            let scenario: task::ZedDebugConfig =
                serde_json::from_value(request.zed_scenario.context("missing zed_scenario")?)?;
            Ok(json!(extension.dap_config_to_scenario(scenario).await?))
        }
        "dap_locator_create_scenario" => {
            let build_config: task::TaskTemplate =
                serde_json::from_value(request.build_config.context("missing build_config")?)?;
            Ok(json!(
                extension
                    .dap_locator_create_scenario(
                        request
                            .debug_locator_name
                            .context("missing debug_locator_name")?,
                        build_config,
                        request.resolved_label.context("missing resolved_label")?,
                        request.adapter_name.context("missing adapter_name")?,
                    )
                    .await?
            ))
        }
        "run_dap_locator" => {
            let build_config: ResolvedBuildTaskRequest =
                serde_json::from_value(request.build_config.context("missing build_config")?)?;
            let build_config = task::SpawnInTerminal {
                label: build_config.label,
                command: build_config.command,
                args: build_config.args,
                env: build_config.env,
                cwd: build_config.cwd,
                ..Default::default()
            };
            Ok(json!(
                extension
                    .run_dap_locator(
                        request
                            .debug_locator_name
                            .context("missing debug_locator_name")?,
                        build_config,
                    )
                    .await?
            ))
        }
        method => bail!("unsupported extension runtime method {method}"),
    }
}

fn main() {
    let result = (|| -> Result<()> {
        struct PendingRequest {
            request: Request,
            response: std::sync::mpsc::SyncSender<Value>,
        }

        let (request_tx, request_rx) = async_channel::unbounded::<PendingRequest>();
        let reader = thread::spawn(move || -> Result<()> {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let request = match serde_json::from_str::<Request>(&line) {
                    Ok(request) => request,
                    Err(error) => {
                        let response =
                            json!({"ok": false, "error": format!("invalid request: {error:#}")});
                        let mut stdout = std::io::stdout().lock();
                        writeln!(
                            stdout,
                            "{RESPONSE_PREFIX}{}",
                            serde_json::to_string(&response)?
                        )?;
                        stdout.flush()?;
                        continue;
                    }
                };
                let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
                if request_tx
                    .send_blocking(PendingRequest {
                        request,
                        response: response_tx,
                    })
                    .is_err()
                {
                    break;
                }
                let response = response_rx
                    .recv()
                    .context("extension runtime response channel closed")?;
                let mut stdout = std::io::stdout().lock();
                writeln!(
                    stdout,
                    "{RESPONSE_PREFIX}{}",
                    serde_json::to_string(&response)?
                )?;
                stdout.flush()?;
            }
            Ok(())
        });
        let app = gpui_platform::headless()
            .with_http_client(Arc::new(ReqwestClient::user_agent(USER_AGENT)?));
        app.run(move |cx| {
            gpui_tokio::init(cx);
            settings::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            cx.spawn(async move |cx| {
                let mut loaded_extension: Option<(PathBuf, WasmExtension)> = None;
                while let Ok(pending) = request_rx.recv().await {
                    let extension_dir = pending.request.extension_dir.clone();
                    let response = async {
                        let canonical_dir = extension_dir
                            .canonicalize()
                            .context("invalid extension directory")?;
                        if loaded_extension
                            .as_ref()
                            .is_some_and(|(loaded_dir, _)| loaded_dir != &canonical_dir)
                        {
                            bail!("persistent runtime cannot switch extension directories");
                        }
                        if loaded_extension.is_none() {
                            let extension = load_extension(&pending.request, cx).await?;
                            loaded_extension = Some((canonical_dir, extension));
                        }
                        let extension = &loaded_extension
                            .as_ref()
                            .context("extension runtime did not load")?
                            .1;
                        execute(extension, pending.request).await
                    }
                    .await;
                    let response = match response {
                        Ok(result) => json!({"ok": true, "result": result}),
                        Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
                    };
                    if pending.response.send(response).is_err() {
                        break;
                    }
                }
                loaded_extension.take();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("extension runtime reader thread panicked"))??;
        Ok(())
    })();
    if let Err(error) = result {
        println!(
            "{RESPONSE_PREFIX}{}",
            json!({"ok": false, "error": format!("{error:#}")})
        );
        std::process::exit(1);
    }
}
