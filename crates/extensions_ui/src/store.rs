#[cfg(not(target_family = "wasm"))]
pub use extension_host::{
    Event, ExtensionManifest, ExtensionOperation, ExtensionStore, is_version_compatible,
};

#[cfg(target_family = "wasm")]
mod remote {
    use std::{
        borrow::Cow,
        collections::BTreeMap as StdBTreeMap,
        ops::Range,
        path::PathBuf,
        sync::{Arc, RwLock},
    };

    use anyhow::{Result, anyhow};
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use cloud_api_types::{ExtensionMetadata, ExtensionProvides};
    use collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    use context_server::ContextServerCommand;
    use dap::{
        DapLocator, DapRegistry, DebugRequest, StartDebuggingRequestArguments,
        StartDebuggingRequestArgumentsRequest,
        adapters::{
            DapDelegate, DebugAdapter, DebugAdapterBinary, DebugAdapterName, DebugTaskDefinition,
            TcpArguments,
        },
    };
    pub use extension::ExtensionManifest;
    use futures::{FutureExt as _, StreamExt as _, channel::mpsc, lock::OwnedMutexGuard};
    use gpui::{
        App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, EventEmitter, Global,
        SharedString, Task, TaskExt as _, UpdateGlobal as _,
    };
    use language::{
        CodeLabel, DynLspInstaller, HighlightId, Language, LanguageConfig, LanguageName,
        LanguageQueries, LanguageRegistry, LanguageServerBinaryLocations, LoadedLanguage,
        LspAdapter, LspAdapterDelegate, QUERY_FILENAME_PREFIXES, Toolchain,
    };
    use language_model::LanguageModelRegistry;
    use lsp::{
        CodeActionKind, LanguageServerBinary, LanguageServerBinaryOptions, LanguageServerName, Uri,
    };
    use project::{
        ContextProviderWithTasks,
        context_server_store::registry::{
            ContextServerDescriptor, ContextServerDescriptorRegistry,
        },
        worktree_store::WorktreeStore,
    };
    use release_channel::ReleaseChannel;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use settings::{SemanticTokenRules, SettingsStore};
    use task::TaskTemplates;
    use wasm_remote::RemoteClient;

    pub type ExtensionAssetMap = Arc<RwLock<StdBTreeMap<String, Vec<u8>>>>;

    #[derive(Clone, Copy)]
    pub enum ExtensionOperation {
        Upgrade,
        Install,
        Remove,
    }

    #[derive(Clone)]
    pub enum Event {
        ExtensionsUpdated,
        StartedReloading,
        ExtensionInstalled(Arc<str>),
        ExtensionUninstalled(Arc<str>),
        ExtensionFailedToLoad(Arc<str>),
    }

    #[derive(Clone)]
    pub struct ExtensionIndexEntry {
        pub manifest: Arc<ExtensionManifest>,
        pub dev: bool,
    }

    #[derive(Deserialize)]
    struct InstalledExtensionResponse {
        id: String,
        #[serde(default)]
        manifest_toml: String,
        #[serde(default)]
        dev: bool,
    }

    #[derive(Deserialize)]
    struct ListResponse {
        #[serde(default)]
        extensions: Vec<InstalledExtensionResponse>,
    }

    #[derive(Deserialize)]
    struct ExtensionResponse {
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        id: String,
    }

    #[derive(Deserialize)]
    struct FetchResponse {
        #[serde(default)]
        data: Vec<ExtensionMetadata>,
    }

    #[derive(Default, Deserialize)]
    struct ContributionsResponse {
        #[serde(default)]
        extensions: Vec<ExtensionContributionsResponse>,
    }

    #[derive(Deserialize)]
    struct ExtensionContributionsResponse {
        id: String,
        #[serde(default)]
        themes: Vec<ThemeContributionResponse>,
        #[serde(default)]
        icon_themes: Vec<IconThemeContributionResponse>,
        #[serde(default)]
        icon_assets: Vec<IconAssetContributionResponse>,
        #[serde(default)]
        languages: Vec<LanguageContributionResponse>,
        #[serde(default)]
        snippets: Vec<SnippetContributionResponse>,
        #[serde(default)]
        grammars: Vec<GrammarContributionResponse>,
        #[serde(default)]
        language_servers: Vec<LanguageServerContributionResponse>,
        #[serde(default)]
        context_servers: Vec<String>,
        #[serde(default)]
        slash_commands: Vec<SlashCommandContributionResponse>,
        #[serde(default)]
        debug_adapters: Vec<DebugAdapterContributionResponse>,
        #[serde(default)]
        debug_locators: Vec<String>,
        #[serde(default)]
        language_model_providers: Vec<LanguageModelProviderContributionResponse>,
    }

    #[derive(Deserialize)]
    struct GrammarContributionResponse {
        id: String,
        content_base64: String,
    }

    #[derive(Deserialize)]
    struct SlashCommandContributionResponse {
        name: String,
        description: String,
        requires_argument: bool,
    }

    #[derive(Deserialize)]
    struct LanguageModelProviderContributionResponse {
        id: String,
        name: String,
        icon: Option<String>,
    }

    #[derive(Deserialize)]
    struct LanguageServerContributionResponse {
        id: String,
        #[serde(default)]
        languages: Vec<String>,
        #[serde(default)]
        language_ids: HashMap<String, String>,
        #[serde(default)]
        code_action_kinds: Option<Vec<CodeActionKind>>,
    }

    #[derive(Deserialize)]
    struct DebugAdapterContributionResponse {
        id: String,
        #[serde(default)]
        schema: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct ThemeContributionResponse {
        #[allow(dead_code)]
        relative_path: String,
        content: String,
    }

    #[derive(Deserialize)]
    struct IconThemeContributionResponse {
        #[allow(dead_code)]
        relative_path: String,
        content: String,
    }

    #[derive(Deserialize)]
    struct IconAssetContributionResponse {
        relative_path: String,
        content: String,
    }

    #[derive(Deserialize)]
    struct LanguageContributionResponse {
        #[allow(dead_code)]
        relative_path: String,
        config: String,
        #[serde(default)]
        queries: HashMap<String, String>,
        #[serde(default)]
        tasks: Option<String>,
        #[serde(default)]
        semantic_token_rules: Option<String>,
    }

    #[derive(Deserialize)]
    struct SnippetContributionResponse {
        relative_path: String,
        content: String,
    }

    #[derive(Deserialize)]
    struct RuntimeResponse<T> {
        #[serde(default)]
        ok: bool,
        result: Option<T>,
        #[serde(default)]
        error: Option<String>,
    }

    fn language_queries_from_response(query_files: &HashMap<String, String>) -> LanguageQueries {
        let mut queries = LanguageQueries::default();
        for (file_name, content) in query_files {
            for (prefix, accessor) in QUERY_FILENAME_PREFIXES {
                if file_name.starts_with(prefix) && file_name.ends_with(".scm") {
                    match accessor(&mut queries) {
                        Some(existing) => existing.to_mut().push_str(content),
                        slot @ None => *slot = Some(Cow::Owned(content.clone())),
                    }
                    break;
                }
            }
        }
        queries
    }

    #[derive(Deserialize)]
    struct LanguageServerCommandResponse {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    }

    #[derive(Deserialize)]
    struct RemoteCodeLabel {
        code: String,
        spans: Vec<RemoteCodeLabelSpan>,
        filter_start: usize,
        filter_end: usize,
    }

    #[derive(Deserialize)]
    struct RemoteCodeLabelSpan {
        kind: String,
        #[serde(default)]
        start: usize,
        #[serde(default)]
        end: usize,
        #[serde(default)]
        text: String,
        highlight_name: Option<String>,
    }

    fn serialized_i32(value: impl Serialize) -> i32 {
        serde_json::to_value(value)
            .ok()
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(-1)
    }

    fn symbol_kind_i32(kind: language::SymbolKind) -> i32 {
        match kind {
            language::SymbolKind::File => 1,
            language::SymbolKind::Module => 2,
            language::SymbolKind::Namespace => 3,
            language::SymbolKind::Package => 4,
            language::SymbolKind::Class => 5,
            language::SymbolKind::Method => 6,
            language::SymbolKind::Property => 7,
            language::SymbolKind::Field => 8,
            language::SymbolKind::Constructor => 9,
            language::SymbolKind::Enum => 10,
            language::SymbolKind::Interface => 11,
            language::SymbolKind::Function => 12,
            language::SymbolKind::Variable => 13,
            language::SymbolKind::Constant => 14,
            language::SymbolKind::String => 15,
            language::SymbolKind::Number => 16,
            language::SymbolKind::Boolean => 17,
            language::SymbolKind::Array => 18,
            language::SymbolKind::Object => 19,
            language::SymbolKind::Key => 20,
            language::SymbolKind::Null => 21,
            language::SymbolKind::EnumMember => 22,
            language::SymbolKind::Struct => 23,
            language::SymbolKind::Event => 24,
            language::SymbolKind::Operator => 25,
            language::SymbolKind::TypeParameter => 26,
        }
    }

    fn build_remote_code_label(
        label: RemoteCodeLabel,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        let parsed_runs = if label.code.is_empty() {
            Vec::new()
        } else {
            language.highlight_text(&label.code.as_str().into(), 0..label.code.len())
        };
        let mut text = String::new();
        let mut runs = Vec::<(Range<usize>, HighlightId)>::new();
        for span in label.spans {
            if span.kind == "code" {
                let range = span.start..span.end;
                let code_span = label.code.get(range.clone())?;
                let output_start = text.len();
                for (run_range, id) in &parsed_runs {
                    let start = run_range.start.max(range.start);
                    let end = run_range.end.min(range.end);
                    if start < end {
                        let mapped_start = output_start + start - range.start;
                        runs.push((mapped_start..mapped_start + end - start, *id));
                    }
                }
                text.push_str(code_span);
            } else if span.kind == "literal" {
                if let Some(highlight_id) = language
                    .grammar()
                    .zip(span.highlight_name.as_ref())
                    .and_then(|(grammar, name)| grammar.highlight_id_for_name(name))
                {
                    let start = text.len();
                    runs.push((start..start + span.text.len(), highlight_id));
                }
                text.push_str(&span.text);
            }
        }
        let filter_range = label.filter_start..label.filter_end;
        text.get(filter_range.clone())?;
        Some(CodeLabel::new(text, filter_range, runs))
    }

    #[derive(Deserialize)]
    struct RemoteTcpArguments {
        host: std::net::IpAddr,
        port: u16,
        timeout: Option<u64>,
    }

    #[derive(Deserialize)]
    struct DebugAdapterBinaryResponse {
        command: Option<String>,
        #[serde(default)]
        arguments: Vec<String>,
        #[serde(default)]
        envs: HashMap<String, String>,
        cwd: Option<PathBuf>,
        connection: Option<RemoteTcpArguments>,
        request_args: StartDebuggingRequestArguments,
    }

    #[derive(Deserialize)]
    struct ContextServerCommandResponse {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    }

    #[derive(Deserialize)]
    struct ContextServerConfigurationResponse {
        installation_instructions: String,
        settings_schema: serde_json::Value,
        default_settings: String,
    }

    struct RemoteExtensionContextServer {
        client: RemoteClient,
        extension_id: Arc<str>,
        context_server_id: Arc<str>,
    }

    impl RemoteExtensionContextServer {
        async fn call<T: serde::de::DeserializeOwned>(
            &self,
            method: &str,
            worktree_ids: Vec<u64>,
        ) -> Result<Option<T>> {
            let response: RuntimeResponse<T> = self
                .client
                .call(
                    "Extensions::runtime_call",
                    &json!({
                        "extension_id": self.extension_id,
                        "method": method,
                        "context_server_id": self.context_server_id,
                        "worktree_ids": worktree_ids,
                    }),
                )
                .await?;
            if !response.ok {
                return Err(anyhow!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "extension runtime call failed".into())
                ));
            }
            Ok(response.result)
        }
    }

    impl ContextServerDescriptor for RemoteExtensionContextServer {
        fn command(
            &self,
            worktree_store: Entity<WorktreeStore>,
            cx: &AsyncApp,
        ) -> Task<Result<ContextServerCommand>> {
            let this = Self {
                client: self.client.clone(),
                extension_id: self.extension_id.clone(),
                context_server_id: self.context_server_id.clone(),
            };
            cx.spawn(async move |cx| {
                let worktree_ids = worktree_store.update(cx, |store, cx| {
                    store
                        .visible_worktrees(cx)
                        .map(|worktree| worktree.read(cx).id().to_proto())
                        .collect::<Vec<_>>()
                });
                let command = this
                    .call::<ContextServerCommandResponse>("context_server_command", worktree_ids)
                    .await?
                    .ok_or_else(|| anyhow!("extension returned no context server command"))?;
                Ok(ContextServerCommand {
                    path: command.command,
                    args: command.args,
                    env: Some(command.env.into_iter().collect()),
                    timeout: None,
                })
            })
        }

        fn configuration(
            &self,
            worktree_store: Entity<WorktreeStore>,
            cx: &AsyncApp,
        ) -> Task<Result<Option<extension::ContextServerConfiguration>>> {
            let this = Self {
                client: self.client.clone(),
                extension_id: self.extension_id.clone(),
                context_server_id: self.context_server_id.clone(),
            };
            cx.spawn(async move |cx| {
                let worktree_ids = worktree_store.update(cx, |store, cx| {
                    store
                        .visible_worktrees(cx)
                        .map(|worktree| worktree.read(cx).id().to_proto())
                        .collect::<Vec<_>>()
                });
                Ok(this
                    .call::<ContextServerConfigurationResponse>(
                        "context_server_configuration",
                        worktree_ids,
                    )
                    .await?
                    .map(|configuration| extension::ContextServerConfiguration {
                        installation_instructions: configuration.installation_instructions,
                        settings_schema: configuration.settings_schema,
                        default_settings: configuration.default_settings,
                    }))
            })
        }
    }

    struct RemoteExtensionLspAdapter {
        client: RemoteClient,
        extension_id: Arc<str>,
        language_server_id: LanguageServerName,
        language_name: LanguageName,
        language_ids: HashMap<LanguageName, String>,
        code_action_kinds: Option<Vec<CodeActionKind>>,
    }

    impl RemoteExtensionLspAdapter {
        async fn call_payload<T: serde::de::DeserializeOwned>(
            &self,
            method: &str,
            payload: serde_json::Value,
        ) -> Result<Option<T>> {
            let mut params = payload.as_object().cloned().unwrap_or_default();
            params.insert(
                "extension_id".into(),
                serde_json::Value::String(self.extension_id.to_string()),
            );
            params.insert(
                "method".into(),
                serde_json::Value::String(method.to_string()),
            );
            params.insert(
                "language_server_id".into(),
                serde_json::Value::String(self.language_server_id.to_string()),
            );
            params.insert(
                "language_name".into(),
                serde_json::Value::String(self.language_name.to_string()),
            );
            let response: RuntimeResponse<T> = self
                .client
                .call(
                    "Extensions::runtime_call",
                    &serde_json::Value::Object(params),
                )
                .await?;
            if !response.ok {
                return Err(anyhow!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "extension runtime call failed".into())
                ));
            }
            Ok(response.result)
        }

        async fn call_for_worktree<T: serde::de::DeserializeOwned>(
            &self,
            method: &str,
            target_language_server_id: Option<&LanguageServerName>,
            delegate: &Arc<dyn LspAdapterDelegate>,
        ) -> Result<Option<T>> {
            let worktree_env = delegate.shell_env().await;
            self.call_payload(
                method,
                json!({
                    "target_language_server_id": target_language_server_id,
                    "worktree_id": delegate.worktree_id().to_proto(),
                    "worktree_root": delegate.worktree_root_path(),
                    "worktree_env": worktree_env,
                }),
            )
            .await
        }
    }

    fn parse_optional_json(value: Option<serde_json::Value>) -> Result<Option<serde_json::Value>> {
        match value {
            Some(serde_json::Value::String(json)) => Ok(Some(serde_json::from_str(&json)?)),
            Some(serde_json::Value::Null) | None => Ok(None),
            value => Ok(value),
        }
    }

    #[async_trait::async_trait(?Send)]
    impl DynLspInstaller for RemoteExtensionLspAdapter {
        fn get_language_server_command(
            self: Arc<Self>,
            delegate: Arc<dyn LspAdapterDelegate>,
            _: Option<Toolchain>,
            _: LanguageServerBinaryOptions,
            _: OwnedMutexGuard<Option<(bool, LanguageServerBinary)>>,
            _: gpui::AsyncApp,
        ) -> LanguageServerBinaryLocations {
            async move {
                let result = self
                    .call_for_worktree::<LanguageServerCommandResponse>(
                        "language_server_command",
                        None,
                        &delegate,
                    )
                    .await
                    .and_then(|command| {
                        let command =
                            command.ok_or_else(|| anyhow!("extension returned no LSP command"))?;
                        Ok(LanguageServerBinary {
                            path: command.command,
                            arguments: command.args.into_iter().map(Into::into).collect(),
                            env: Some(command.env.into_iter().collect()),
                        })
                    });
                (result, None)
            }
            .boxed_local()
        }

        async fn try_fetch_server_binary(
            &self,
            _: &Arc<dyn LspAdapterDelegate>,
            _: PathBuf,
            _: bool,
            _: &mut gpui::AsyncApp,
        ) -> Result<LanguageServerBinary> {
            unreachable!("remote extension LSP commands are resolved by the server runtime")
        }
    }

    #[async_trait::async_trait(?Send)]
    impl LspAdapter for RemoteExtensionLspAdapter {
        fn name(&self) -> LanguageServerName {
            self.language_server_id.clone()
        }

        fn is_extension(&self) -> bool {
            true
        }

        fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
            self.code_action_kinds.clone().or(Some(vec![
                CodeActionKind::EMPTY,
                CodeActionKind::QUICKFIX,
                CodeActionKind::REFACTOR,
                CodeActionKind::REFACTOR_EXTRACT,
                CodeActionKind::SOURCE,
            ]))
        }

        fn language_ids(&self) -> HashMap<LanguageName, String> {
            self.language_ids.clone()
        }

        async fn labels_for_completions(
            self: Arc<Self>,
            completions: &[lsp::CompletionItem],
            language: &Arc<Language>,
        ) -> Result<Vec<Option<CodeLabel>>> {
            let completions = completions
                .iter()
                .map(|completion| {
                    json!({
                        "label": &completion.label,
                        "label_details": &completion.label_details,
                        "detail": &completion.detail,
                        "kind": completion.kind.map(serialized_i32),
                        "insert_text_format": completion.insert_text_format.map(serialized_i32),
                    })
                })
                .collect::<Vec<_>>();
            let labels = self
                .call_payload::<Vec<Option<RemoteCodeLabel>>>(
                    "labels_for_completions",
                    json!({ "completions": completions }),
                )
                .await?
                .unwrap_or_default();
            Ok(labels
                .into_iter()
                .map(|label| label.and_then(|label| build_remote_code_label(label, language)))
                .collect())
        }

        async fn labels_for_symbols(
            self: Arc<Self>,
            symbols: &[language::Symbol],
            language: &Arc<Language>,
        ) -> Result<Vec<Option<CodeLabel>>> {
            let symbols = symbols
                .iter()
                .map(|symbol| {
                    json!({
                        "kind": symbol_kind_i32(symbol.kind),
                        "name": &symbol.name,
                        "container_name": &symbol.container_name,
                    })
                })
                .collect::<Vec<_>>();
            let labels = self
                .call_payload::<Vec<Option<RemoteCodeLabel>>>(
                    "labels_for_symbols",
                    json!({ "symbols": symbols }),
                )
                .await?
                .unwrap_or_default();
            Ok(labels
                .into_iter()
                .map(|label| label.and_then(|label| build_remote_code_label(label, language)))
                .collect())
        }

        async fn initialization_options(
            self: Arc<Self>,
            delegate: &Arc<dyn LspAdapterDelegate>,
            _: &mut gpui::AsyncApp,
        ) -> Result<Option<serde_json::Value>> {
            let value = self
                .call_for_worktree::<serde_json::Value>(
                    "language_server_initialization_options",
                    None,
                    delegate,
                )
                .await?;
            parse_optional_json(value)
        }

        async fn workspace_configuration(
            self: Arc<Self>,
            delegate: &Arc<dyn LspAdapterDelegate>,
            _: Option<Toolchain>,
            _: Option<Uri>,
            _: &mut gpui::AsyncApp,
        ) -> Result<serde_json::Value> {
            let value = self
                .call_for_worktree::<serde_json::Value>(
                    "language_server_workspace_configuration",
                    None,
                    delegate,
                )
                .await?;
            Ok(parse_optional_json(value)?.unwrap_or_else(|| json!({})))
        }

        async fn initialization_options_schema(
            self: Arc<Self>,
            delegate: &Arc<dyn LspAdapterDelegate>,
            _: OwnedMutexGuard<Option<(bool, LanguageServerBinary)>>,
            _: &mut gpui::AsyncApp,
        ) -> Option<serde_json::Value> {
            self.call_for_worktree::<serde_json::Value>(
                "language_server_initialization_options_schema",
                None,
                delegate,
            )
            .await
            .and_then(parse_optional_json)
            .ok()
            .flatten()
        }

        async fn settings_schema(
            self: Arc<Self>,
            delegate: &Arc<dyn LspAdapterDelegate>,
            _: OwnedMutexGuard<Option<(bool, LanguageServerBinary)>>,
            _: &mut gpui::AsyncApp,
        ) -> Option<serde_json::Value> {
            self.call_for_worktree::<serde_json::Value>(
                "language_server_workspace_configuration_schema",
                None,
                delegate,
            )
            .await
            .and_then(parse_optional_json)
            .ok()
            .flatten()
        }

        async fn additional_initialization_options(
            self: Arc<Self>,
            target: LanguageServerName,
            delegate: &Arc<dyn LspAdapterDelegate>,
        ) -> Result<Option<serde_json::Value>> {
            let value = self
                .call_for_worktree::<serde_json::Value>(
                    "language_server_additional_initialization_options",
                    Some(&target),
                    delegate,
                )
                .await?;
            parse_optional_json(value)
        }

        async fn additional_workspace_configuration(
            self: Arc<Self>,
            target: LanguageServerName,
            delegate: &Arc<dyn LspAdapterDelegate>,
            _: &mut gpui::AsyncApp,
        ) -> Result<Option<serde_json::Value>> {
            let value = self
                .call_for_worktree::<serde_json::Value>(
                    "language_server_additional_workspace_configuration",
                    Some(&target),
                    delegate,
                )
                .await?;
            parse_optional_json(value)
        }
    }

    struct RemoteExtensionDebugAdapter {
        client: RemoteClient,
        extension_id: Arc<str>,
        debug_adapter_name: Arc<str>,
        schema: serde_json::Value,
    }

    impl RemoteExtensionDebugAdapter {
        async fn call<T: serde::de::DeserializeOwned>(
            &self,
            method: &str,
            payload: serde_json::Value,
        ) -> Result<T> {
            let mut params = payload.as_object().cloned().unwrap_or_default();
            params.insert(
                "extension_id".into(),
                serde_json::Value::String(self.extension_id.to_string()),
            );
            params.insert(
                "debug_adapter_name".into(),
                serde_json::Value::String(self.debug_adapter_name.to_string()),
            );
            params.insert(
                "method".into(),
                serde_json::Value::String(method.to_string()),
            );
            let response: RuntimeResponse<T> = self
                .client
                .call(
                    "Extensions::runtime_call",
                    &serde_json::Value::Object(params),
                )
                .await?;
            if !response.ok {
                return Err(anyhow!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "extension runtime call failed".into())
                ));
            }
            response
                .result
                .ok_or_else(|| anyhow!("extension runtime returned no result"))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl DebugAdapter for RemoteExtensionDebugAdapter {
        fn name(&self) -> DebugAdapterName {
            DebugAdapterName(self.debug_adapter_name.to_string().into())
        }

        fn dap_schema(&self) -> serde_json::Value {
            self.schema.clone()
        }

        async fn get_binary(
            &self,
            delegate: &Arc<dyn DapDelegate>,
            definition: &DebugTaskDefinition,
            user_installed_path: Option<PathBuf>,
            _: Option<Vec<String>>,
            _: Option<HashMap<String, String>>,
            _: &mut gpui::AsyncApp,
        ) -> Result<DebugAdapterBinary> {
            let response: DebugAdapterBinaryResponse = self
                .call(
                    "get_dap_binary",
                    json!({
                        "label": definition.label,
                        "config": definition.config,
                        "tcp_connection": definition.tcp_connection,
                        "user_installed_path": user_installed_path,
                        "worktree_id": delegate.worktree_id().to_proto(),
                        "worktree_root": delegate.worktree_root_path(),
                        "worktree_env": delegate.shell_env().await,
                    }),
                )
                .await?;
            Ok(DebugAdapterBinary {
                command: response.command,
                arguments: response.arguments,
                envs: response.envs,
                cwd: response.cwd,
                connection: response.connection.map(|connection| TcpArguments {
                    host: connection.host,
                    port: connection.port,
                    timeout: connection.timeout,
                }),
                request_args: response.request_args,
            })
        }

        async fn config_from_zed_format(
            &self,
            scenario: task::ZedDebugConfig,
        ) -> Result<task::DebugScenario> {
            self.call(
                "dap_config_to_scenario",
                json!({ "zed_scenario": scenario }),
            )
            .await
        }

        async fn request_kind(
            &self,
            config: &serde_json::Value,
        ) -> Result<StartDebuggingRequestArgumentsRequest> {
            self.call("dap_request_kind", json!({ "config": config }))
                .await
        }
    }

    struct RemoteExtensionDebugLocator {
        client: RemoteClient,
        extension_id: Arc<str>,
        locator_name: SharedString,
    }

    impl RemoteExtensionDebugLocator {
        async fn call<T: serde::de::DeserializeOwned>(
            &self,
            method: &str,
            payload: serde_json::Value,
        ) -> Result<Option<T>> {
            let mut params = payload.as_object().cloned().unwrap_or_default();
            params.insert(
                "extension_id".into(),
                serde_json::Value::String(self.extension_id.to_string()),
            );
            params.insert(
                "debug_locator_name".into(),
                serde_json::Value::String(self.locator_name.to_string()),
            );
            params.insert(
                "method".into(),
                serde_json::Value::String(method.to_string()),
            );
            let response: RuntimeResponse<T> = self
                .client
                .call(
                    "Extensions::runtime_call",
                    &serde_json::Value::Object(params),
                )
                .await?;
            if !response.ok {
                return Err(anyhow!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "extension runtime call failed".into())
                ));
            }
            Ok(response.result)
        }
    }

    #[async_trait::async_trait]
    impl DapLocator for RemoteExtensionDebugLocator {
        fn name(&self) -> SharedString {
            self.locator_name.clone()
        }

        async fn create_scenario(
            &self,
            build_config: &task::TaskTemplate,
            resolved_label: &str,
            adapter: &DebugAdapterName,
        ) -> Option<task::DebugScenario> {
            self.call(
                "dap_locator_create_scenario",
                json!({
                    "build_config": build_config,
                    "resolved_label": resolved_label,
                    "adapter_name": adapter,
                }),
            )
            .await
            .ok()
            .flatten()
        }

        async fn run(
            &self,
            build_config: task::SpawnInTerminal,
            _: BackgroundExecutor,
        ) -> Result<DebugRequest> {
            self.call(
                "run_dap_locator",
                json!({
                    "build_config": {
                        "label": build_config.label,
                        "command": build_config.command,
                        "args": build_config.args,
                        "env": build_config.env,
                        "cwd": build_config.cwd,
                    }
                }),
            )
            .await?
            .ok_or_else(|| anyhow!("extension runtime returned no debug request"))
        }
    }

    pub struct ExtensionStore {
        client: RemoteClient,
        languages: Arc<LanguageRegistry>,
        extension_assets: ExtensionAssetMap,
        installed: BTreeMap<Arc<str>, ExtensionIndexEntry>,
        outstanding: BTreeMap<Arc<str>, ExtensionOperation>,
        extension_themes: BTreeMap<Arc<str>, Vec<Arc<str>>>,
        extension_icon_themes: BTreeMap<Arc<str>, Vec<Arc<str>>>,
        extension_icon_assets: BTreeMap<Arc<str>, Vec<String>>,
        extension_languages: BTreeMap<Arc<str>, Vec<LanguageName>>,
        extension_grammars: BTreeMap<Arc<str>, Vec<Arc<str>>>,
        extension_snippets: BTreeMap<Arc<str>, Vec<PathBuf>>,
        extension_language_servers: BTreeMap<Arc<str>, Vec<(LanguageName, LanguageServerName)>>,
        extension_context_servers: BTreeMap<Arc<str>, Vec<Arc<str>>>,
        extension_debug_adapters: BTreeMap<Arc<str>, Vec<Arc<str>>>,
        extension_debug_locators: BTreeMap<Arc<str>, Vec<SharedString>>,
        extension_language_model_providers: BTreeMap<Arc<str>, Vec<Arc<str>>>,
    }

    impl EventEmitter<Event> for ExtensionStore {}

    struct GlobalExtensionStore(Entity<ExtensionStore>);
    impl Global for GlobalExtensionStore {}

    impl ExtensionStore {
        pub fn init_remote(
            client: RemoteClient,
            languages: Arc<LanguageRegistry>,
            extension_assets: ExtensionAssetMap,
            cx: &mut App,
        ) {
            let notification_client = client.clone();
            let store = cx.new(|_| Self {
                client,
                languages,
                extension_assets,
                installed: BTreeMap::default(),
                outstanding: BTreeMap::default(),
                extension_themes: BTreeMap::default(),
                extension_icon_themes: BTreeMap::default(),
                extension_icon_assets: BTreeMap::default(),
                extension_languages: BTreeMap::default(),
                extension_grammars: BTreeMap::default(),
                extension_snippets: BTreeMap::default(),
                extension_language_servers: BTreeMap::default(),
                extension_context_servers: BTreeMap::default(),
                extension_debug_adapters: BTreeMap::default(),
                extension_debug_locators: BTreeMap::default(),
                extension_language_model_providers: BTreeMap::default(),
            });
            cx.set_global(GlobalExtensionStore(store.clone()));
            let (refresh_tx, mut refresh_rx) = mpsc::unbounded::<()>();
            notification_client.on_notification("Host::extensions_changed", move |_| {
                if refresh_tx.unbounded_send(()).is_err() {
                    log::debug!("extension refresh listener has closed");
                }
            });
            let weak_store = store.downgrade();
            cx.spawn(async move |cx| {
                while refresh_rx.next().await.is_some() {
                    if weak_store
                        .update(cx, |store, cx| store.refresh(cx))
                        .is_err()
                    {
                        break;
                    }
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            store.update(cx, |store, cx| store.refresh(cx));
        }

        pub fn global(cx: &App) -> Entity<Self> {
            cx.global::<GlobalExtensionStore>().0.clone()
        }

        fn parse_installed(
            extensions: Vec<InstalledExtensionResponse>,
        ) -> BTreeMap<Arc<str>, ExtensionIndexEntry> {
            extensions
                .into_iter()
                .filter_map(|extension| {
                    let manifest =
                        toml::from_str::<ExtensionManifest>(&extension.manifest_toml).ok()?;
                    Some((
                        Arc::from(extension.id),
                        ExtensionIndexEntry {
                            manifest: Arc::new(manifest),
                            dev: extension.dev,
                        },
                    ))
                })
                .collect()
        }

        fn refresh(&mut self, cx: &mut Context<Self>) {
            let client = self.client.clone();
            cx.spawn(async move |this, cx| {
                let response: ListResponse = client.call("Extensions::list", &json!({})).await?;
                let contributions: ContributionsResponse = client
                    .call("Extensions::contributions", &json!({}))
                    .await
                    .unwrap_or_default();
                this.update(cx, |this, cx| {
                    this.installed = Self::parse_installed(response.extensions);
                    this.apply_contributions(contributions, cx);
                    cx.emit(Event::ExtensionsUpdated);
                    cx.notify();
                })
            })
            .detach_and_log_err(cx);
        }

        fn apply_contributions(
            &mut self,
            contributions: ContributionsResponse,
            cx: &mut Context<Self>,
        ) {
            let registry = theme::ThemeRegistry::global(cx);
            let old_names = self
                .extension_themes
                .values()
                .flatten()
                .map(|name| name.as_ref().into())
                .collect::<Vec<_>>();
            registry.remove_user_themes(&old_names);
            self.extension_themes.clear();
            let old_icon_theme_names = self
                .extension_icon_themes
                .values()
                .flatten()
                .map(|name| name.as_ref().into())
                .collect::<Vec<_>>();
            registry.remove_icon_themes(&old_icon_theme_names);
            self.extension_icon_themes.clear();
            {
                let mut assets = self
                    .extension_assets
                    .write()
                    .expect("extension asset map lock poisoned");
                for path in self.extension_icon_assets.values().flatten() {
                    assets.remove(path);
                }
            }
            self.extension_icon_assets.clear();
            let old_languages = self
                .extension_languages
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            let old_grammars = self
                .extension_grammars
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            SettingsStore::update_global(cx, |store, cx| {
                for language in &old_languages {
                    store.remove_language_semantic_token_rules(language.as_ref(), cx);
                }
            });
            self.languages
                .remove_languages(&old_languages, &old_grammars);
            self.extension_languages.clear();
            self.extension_grammars.clear();
            let snippet_registry = snippet_provider::SnippetRegistry::global(cx);
            for path in self.extension_snippets.values().flatten() {
                snippet_registry.unregister_snippets(path);
            }
            self.extension_snippets.clear();
            for (language, server) in self.extension_language_servers.values().flatten() {
                self.languages.remove_lsp_adapter(language, server);
            }
            self.extension_language_servers.clear();
            let context_server_registry = ContextServerDescriptorRegistry::default_global(cx);
            for server_id in self.extension_context_servers.values().flatten() {
                context_server_registry.update(cx, |registry, cx| {
                    registry.unregister_context_server_descriptor_by_id(server_id, cx);
                });
            }
            self.extension_context_servers.clear();
            let dap_registry = DapRegistry::global(cx).clone();
            for name in self.extension_debug_adapters.values().flatten() {
                dap_registry.remove_adapter(name);
            }
            self.extension_debug_adapters.clear();
            for name in self.extension_debug_locators.values().flatten() {
                dap_registry.remove_locator(name);
            }
            self.extension_debug_locators.clear();
            self.extension_language_model_providers.clear();
            let mut semantic_token_rules_to_add = Vec::new();

            for extension in contributions.extensions {
                let extension_id: Arc<str> = Arc::from(extension.id);
                let mut names = Vec::new();
                for contribution in extension.themes {
                    let Ok(theme_family) =
                        theme_settings::deserialize_user_theme(contribution.content.as_bytes())
                    else {
                        log::error!(
                            "failed to parse theme contribution for extension {}",
                            extension_id
                        );
                        continue;
                    };
                    names.extend(
                        theme_family
                            .themes
                            .iter()
                            .map(|theme| Arc::from(theme.name.as_str())),
                    );
                    if let Err(error) =
                        theme_settings::load_user_theme(&registry, contribution.content.as_bytes())
                    {
                        log::error!(
                            "failed to load theme contribution for extension {}: {error:#}",
                            extension_id
                        );
                    }
                }
                self.extension_themes.insert(extension_id.clone(), names);

                let asset_root = PathBuf::from("extensions").join(extension_id.as_ref());
                let mut asset_paths = Vec::new();
                {
                    let mut assets = self
                        .extension_assets
                        .write()
                        .expect("extension asset map lock poisoned");
                    for contribution in extension.icon_assets {
                        let path = asset_root
                            .join(contribution.relative_path)
                            .to_string_lossy()
                            .to_string();
                        assets.insert(path.clone(), contribution.content.into_bytes());
                        asset_paths.push(path);
                    }
                }
                self.extension_icon_assets
                    .insert(extension_id.clone(), asset_paths);

                let mut icon_theme_names = Vec::new();
                for contribution in extension.icon_themes {
                    let Ok(icon_theme_family) =
                        theme::deserialize_icon_theme(contribution.content.as_bytes())
                    else {
                        log::error!(
                            "failed to parse icon theme contribution for extension {}",
                            extension_id
                        );
                        continue;
                    };
                    icon_theme_names.extend(
                        icon_theme_family
                            .themes
                            .iter()
                            .map(|icon_theme| Arc::from(icon_theme.name.as_str())),
                    );
                    if let Err(error) = registry.load_icon_theme(icon_theme_family, &asset_root) {
                        log::error!(
                            "failed to load icon theme contribution for extension {}: {error:#}",
                            extension_id
                        );
                    }
                }
                self.extension_icon_themes
                    .insert(extension_id.clone(), icon_theme_names);

                let mut grammar_names = Vec::new();
                let grammar_bytes = extension
                    .grammars
                    .into_iter()
                    .filter_map(|contribution| {
                        let name: Arc<str> = contribution.id.into();
                        match BASE64.decode(contribution.content_base64) {
                            Ok(bytes) => {
                                grammar_names.push(name.clone());
                                Some((name, Arc::<[u8]>::from(bytes)))
                            }
                            Err(error) => {
                                log::error!(
                                    "failed to decode grammar contribution for extension {}: {error:#}",
                                    extension_id
                                );
                                None
                            }
                        }
                    })
                    .collect();
                self.languages.register_wasm_grammar_bytes(grammar_bytes);
                self.extension_grammars
                    .insert(extension_id.clone(), grammar_names);

                let mut language_names = Vec::new();
                for contribution in extension.languages {
                    let Ok(config) = toml::from_str::<LanguageConfig>(&contribution.config) else {
                        log::error!(
                            "failed to parse language contribution for extension {}",
                            extension_id
                        );
                        continue;
                    };
                    let name = config.name.clone();
                    let grammar = config.grammar.clone();
                    let matcher = config.matcher.clone();
                    let hidden = config.hidden;
                    let loaded_config = config.clone();
                    let context_provider = contribution.tasks.as_deref().and_then(|contents| {
                        match settings::parse_json_with_comments::<TaskTemplates>(contents) {
                            Ok(definitions) => Some(
                                Arc::new(ContextProviderWithTasks::new(definitions))
                                    as Arc<_>,
                            ),
                            Err(error) => {
                                log::error!(
                                    "failed to parse task contribution for extension {} language {}: {error:#}",
                                    extension_id,
                                    name
                                );
                                None
                            }
                        }
                    });
                    if let Some(contents) = contribution.semantic_token_rules.as_deref() {
                        match settings::parse_json_with_comments::<SemanticTokenRules>(contents) {
                            Ok(rules) => semantic_token_rules_to_add.push((name.clone(), rules)),
                            Err(error) => {
                                log::error!(
                                    "failed to parse semantic token rules for extension {} language {}: {error:#}",
                                    extension_id,
                                    name
                                );
                            }
                        }
                    }
                    let query_files = contribution.queries;
                    self.languages.register_language(
                        name.clone(),
                        grammar,
                        matcher,
                        hidden,
                        None,
                        Arc::new(move || {
                            Ok(LoadedLanguage {
                                config: loaded_config.clone(),
                                queries: language_queries_from_response(&query_files),
                                context_provider: context_provider.clone(),
                                toolchain_provider: None,
                                manifest_name: None,
                            })
                        }),
                    );
                    language_names.push(name);
                }
                self.extension_languages
                    .insert(extension_id.clone(), language_names);

                let mut snippet_paths = Vec::new();
                for contribution in extension.snippets {
                    let path = PathBuf::from(contribution.relative_path);
                    if let Err(error) =
                        snippet_registry.register_snippets(&path, &contribution.content)
                    {
                        log::error!(
                            "failed to register snippets for extension {}: {error:#}",
                            extension_id
                        );
                        continue;
                    }
                    snippet_paths.push(path);
                }
                self.extension_snippets
                    .insert(extension_id.clone(), snippet_paths);

                let mut registered_servers = Vec::new();
                for server in extension.language_servers {
                    let server_id = LanguageServerName(server.id.into());
                    let language_ids: HashMap<LanguageName, String> = server
                        .language_ids
                        .into_iter()
                        .map(|(name, id)| (LanguageName::new(&name), id))
                        .collect();
                    for language in server.languages {
                        let language = LanguageName::new(&language);
                        self.languages.register_lsp_adapter(
                            language.clone(),
                            Arc::new(RemoteExtensionLspAdapter {
                                client: self.client.clone(),
                                extension_id: extension_id.clone(),
                                language_server_id: server_id.clone(),
                                language_name: language.clone(),
                                language_ids: language_ids.clone(),
                                code_action_kinds: server.code_action_kinds.clone(),
                            }),
                        );
                        registered_servers.push((language, server_id.clone()));
                    }
                }
                self.extension_language_servers
                    .insert(extension_id.clone(), registered_servers);

                let mut registered_context_servers = Vec::new();
                for server_id in extension.context_servers {
                    let server_id: Arc<str> = server_id.into();
                    let client = self.client.clone();
                    context_server_registry.update(cx, |registry, cx| {
                        registry.register_context_server_descriptor(
                            server_id.clone(),
                            Arc::new(RemoteExtensionContextServer {
                                client,
                                extension_id: extension_id.clone(),
                                context_server_id: server_id.clone(),
                            }),
                            cx,
                        );
                    });
                    registered_context_servers.push(server_id);
                }
                self.extension_context_servers
                    .insert(extension_id.clone(), registered_context_servers);

                let mut registered_debug_adapters = Vec::new();
                for adapter in extension.debug_adapters {
                    let name: Arc<str> = adapter.id.into();
                    dap_registry.add_adapter(Arc::new(RemoteExtensionDebugAdapter {
                        client: self.client.clone(),
                        extension_id: extension_id.clone(),
                        debug_adapter_name: name.clone(),
                        schema: adapter.schema,
                    }));
                    registered_debug_adapters.push(name);
                }
                self.extension_debug_adapters
                    .insert(extension_id.clone(), registered_debug_adapters);

                let mut registered_debug_locators = Vec::new();
                for locator in extension.debug_locators {
                    let name: SharedString = locator.into();
                    dap_registry.add_locator(Arc::new(RemoteExtensionDebugLocator {
                        client: self.client.clone(),
                        extension_id: extension_id.clone(),
                        locator_name: name.clone(),
                    }));
                    registered_debug_locators.push(name);
                }
                self.extension_debug_locators
                    .insert(extension_id.clone(), registered_debug_locators);

                if !extension.language_model_providers.is_empty() {
                    self.extension_language_model_providers.insert(
                        extension_id,
                        extension
                            .language_model_providers
                            .into_iter()
                            .map(|provider| Arc::from(provider.id))
                            .collect(),
                    );
                }
            }

            SettingsStore::update_global(cx, |store, cx| {
                for (language, rules) in semantic_token_rules_to_add {
                    store.set_language_semantic_token_rules(language.0, rules, cx);
                }
            });

            let installed_llm_extensions: HashSet<Arc<str>> = self
                .extension_language_model_providers
                .keys()
                .cloned()
                .collect();
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.sync_installed_llm_extensions(installed_llm_extensions, cx);
            });
        }

        pub fn outstanding_operations(&self) -> &BTreeMap<Arc<str>, ExtensionOperation> {
            &self.outstanding
        }

        pub fn installed_extensions(&self) -> &BTreeMap<Arc<str>, ExtensionIndexEntry> {
            &self.installed
        }

        pub fn dev_extensions(&self) -> impl Iterator<Item = &Arc<ExtensionManifest>> {
            self.installed
                .values()
                .filter_map(|extension| extension.dev.then_some(&extension.manifest))
        }

        pub fn extension_manifest_for_id(
            &self,
            extension_id: &str,
        ) -> Option<&Arc<ExtensionManifest>> {
            self.installed
                .get(extension_id)
                .map(|extension| &extension.manifest)
        }

        pub fn extension_themes<'a>(
            &'a self,
            extension_id: &'a str,
        ) -> impl Iterator<Item = &'a Arc<str>> {
            self.extension_themes
                .get(extension_id)
                .into_iter()
                .flatten()
        }

        pub fn extension_icon_themes<'a>(
            &'a self,
            extension_id: &'a str,
        ) -> impl Iterator<Item = &'a Arc<str>> {
            self.extension_icon_themes
                .get(extension_id)
                .into_iter()
                .flatten()
        }

        pub fn fetch_extensions(
            &self,
            search: Option<&str>,
            provides_filter: Option<&BTreeSet<ExtensionProvides>>,
            cx: &mut Context<Self>,
        ) -> Task<Result<Vec<ExtensionMetadata>>> {
            let client = self.client.clone();
            let search = search.unwrap_or_default().to_string();
            let provides = provides_filter
                .map(|filter| filter.iter().map(ToString::to_string).collect::<Vec<_>>());
            cx.spawn(async move |_, _| {
                let response: FetchResponse = client
                    .call(
                        "Extensions::fetch",
                        &json!({ "query": search, "provides": provides }),
                    )
                    .await?;
                Ok(response.data)
            })
        }

        pub fn fetch_extension_versions(
            &self,
            extension_id: &str,
            cx: &mut Context<Self>,
        ) -> Task<Result<Vec<ExtensionMetadata>>> {
            let client = self.client.clone();
            let extension_id = extension_id.to_string();
            cx.spawn(async move |_, _| {
                let response: FetchResponse = client
                    .call("Extensions::versions", &json!({ "id": extension_id }))
                    .await?;
                Ok(response.data)
            })
        }

        fn run_operation(
            &mut self,
            method: &'static str,
            extension_id: Arc<str>,
            version: Option<Arc<str>>,
            operation: ExtensionOperation,
            cx: &mut Context<Self>,
        ) -> Task<Result<()>> {
            if self.outstanding.contains_key(&extension_id) {
                return Task::ready(Ok(()));
            }
            self.outstanding.insert(extension_id.clone(), operation);
            cx.notify();
            let client = self.client.clone();
            cx.spawn(async move |this, cx| {
                let response = client
                    .call(method, &json!({ "id": extension_id, "version": version }))
                    .await;
                let response: ExtensionResponse = match response {
                    Ok(response) => response,
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            this.outstanding.remove(extension_id.as_ref());
                            cx.emit(Event::ExtensionFailedToLoad(extension_id.clone()));
                            cx.notify();
                        })?;
                        return Err(error);
                    }
                };
                if !response.ok {
                    this.update(cx, |this, cx| {
                        this.outstanding.remove(extension_id.as_ref());
                        cx.emit(Event::ExtensionFailedToLoad(extension_id.clone()));
                        cx.notify();
                    })?;
                    return Err(anyhow!(
                        "{}",
                        response
                            .error
                            .unwrap_or_else(|| "extension operation failed".into())
                    ));
                }
                let installed_id: Arc<str> = if response.id.is_empty() {
                    extension_id.clone()
                } else {
                    Arc::from(response.id)
                };
                let list: ListResponse = client.call("Extensions::list", &json!({})).await?;
                let contributions: ContributionsResponse = client
                    .call("Extensions::contributions", &json!({}))
                    .await
                    .unwrap_or_default();
                this.update(cx, |this, cx| {
                    this.outstanding.remove(extension_id.as_ref());
                    this.installed = Self::parse_installed(list.extensions);
                    this.apply_contributions(contributions, cx);
                    if method == "Extensions::uninstall" {
                        cx.emit(Event::ExtensionUninstalled(installed_id));
                    } else {
                        cx.emit(Event::ExtensionInstalled(installed_id));
                    }
                    cx.emit(Event::ExtensionsUpdated);
                    cx.notify();
                })?;
                Ok(())
            })
        }

        pub fn install_latest_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
            self.run_operation(
                "Extensions::install",
                extension_id,
                None,
                ExtensionOperation::Install,
                cx,
            )
            .detach_and_log_err(cx);
        }

        pub fn install_extension(
            &mut self,
            extension_id: Arc<str>,
            version: Arc<str>,
            cx: &mut Context<Self>,
        ) {
            self.run_operation(
                "Extensions::install",
                extension_id,
                Some(version),
                ExtensionOperation::Install,
                cx,
            )
            .detach_and_log_err(cx);
        }

        pub fn upgrade_extension(
            &mut self,
            extension_id: Arc<str>,
            version: Arc<str>,
            cx: &mut Context<Self>,
        ) -> Task<Result<()>> {
            self.run_operation(
                "Extensions::install",
                extension_id,
                Some(version),
                ExtensionOperation::Upgrade,
                cx,
            )
        }

        pub fn uninstall_extension(
            &mut self,
            extension_id: Arc<str>,
            cx: &mut Context<Self>,
        ) -> Task<Result<()>> {
            self.run_operation(
                "Extensions::uninstall",
                extension_id,
                None,
                ExtensionOperation::Remove,
                cx,
            )
        }

        pub fn install_dev_extension(
            &mut self,
            path: PathBuf,
            cx: &mut Context<Self>,
        ) -> Task<Result<()>> {
            let extension_id: Arc<str> = Arc::from(path.to_string_lossy().to_string());
            self.run_operation(
                "Extensions::install_dev",
                extension_id,
                None,
                ExtensionOperation::Install,
                cx,
            )
        }

        pub fn rebuild_dev_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
            self.run_operation(
                "Extensions::rebuild_dev",
                extension_id,
                None,
                ExtensionOperation::Upgrade,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }

    pub fn is_version_compatible(
        _release_channel: ReleaseChannel,
        extension: &ExtensionMetadata,
    ) -> bool {
        extension.manifest.schema_version.unwrap_or(0) <= 1
    }
}

#[cfg(target_family = "wasm")]
pub use remote::*;
