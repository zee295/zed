use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, LazyLock, Mutex, RwLock},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};

use crate::fs_rpc::FsRpc;

const ZED_API: &str = "https://api.zed.dev";
const RESPONSE_PREFIX: &str = "ZED_EXTENSION_RESPONSE ";
const STDERR_TAIL_LINES: usize = 50;
static EXTENSION_LIFECYCLE_LOCK: RwLock<()> = RwLock::new(());
static RUNTIME_WORKERS: LazyLock<Mutex<HashMap<RuntimeWorkerKey, Arc<Mutex<RuntimeWorker>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RuntimeWorkerKey {
    workspace: PathBuf,
    extension_id: String,
}

struct RuntimeProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

struct RuntimeWorker {
    runtime: PathBuf,
    working_directory: PathBuf,
    process: Option<RuntimeProcess>,
}

impl RuntimeWorker {
    fn start(runtime: PathBuf, working_directory: PathBuf) -> Result<Self> {
        let mut worker = Self {
            runtime,
            working_directory,
            process: None,
        };
        worker.restart()?;
        Ok(worker)
    }

    fn call(&mut self, request: &Value) -> Result<Value> {
        let request = serde_json::to_string(request)?;
        let mut first_error = None;
        for attempt in 0..2 {
            if self
                .process
                .as_mut()
                .is_some_and(|process| process.child.try_wait().ok().flatten().is_some())
            {
                self.restart()?;
            }
            match self.call_once(&request) {
                Ok(response) => return Ok(response),
                Err(error) if attempt == 0 => {
                    first_error = Some(error);
                    self.restart()?;
                }
                Err(error) => {
                    let first_error = first_error
                        .map(|first| format!("{first:#}; retry failed: {error:#}"))
                        .unwrap_or_else(|| format!("{error:#}"));
                    bail!("persistent extension runtime call failed: {first_error}");
                }
            }
        }
        unreachable!("extension runtime call loop always returns")
    }

    fn call_once(&mut self, request: &str) -> Result<Value> {
        let process = self
            .process
            .as_mut()
            .context("extension runtime is not running")?;
        writeln!(process.stdin, "{request}").context("writing extension runtime request")?;
        process
            .stdin
            .flush()
            .context("flushing extension runtime request")?;

        let mut line = String::new();
        loop {
            line.clear();
            let bytes = process
                .stdout
                .read_line(&mut line)
                .context("reading extension runtime response")?;
            if bytes == 0 {
                let stderr = stderr_tail(&process.stderr_tail);
                bail!("extension runtime closed stdout: {stderr}");
            }
            if let Some(response) = line.trim_end().strip_prefix(RESPONSE_PREFIX) {
                return serde_json::from_str(response)
                    .context("parsing extension runtime response");
            }
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "extension RPC dispatch runs on Tokio's blocking thread pool"
    )]
    fn restart(&mut self) -> Result<()> {
        self.stop();
        let mut child = Command::new(&self.runtime)
            .current_dir(&self.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "starting persistent extension runtime {}",
                    self.runtime.display()
                )
            })?;
        let stdin = child.stdin.take().context("extension runtime stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("extension runtime stdout")?);
        let stderr = child.stderr.take().context("extension runtime stderr")?;
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let reader_tail = stderr_tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut tail) = reader_tail.lock() {
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            }
        });
        self.process = Some(RuntimeProcess {
            child,
            stdin,
            stdout,
            stderr_tail,
        });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        drop(process.stdin);
        if process.child.try_wait().ok().flatten().is_none() {
            let _ = process.child.kill();
        }
        let _ = process.child.wait();
    }
}

impl Drop for RuntimeWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stderr_tail(tail: &Mutex<VecDeque<String>>) -> String {
    tail.lock()
        .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|_| "stderr unavailable".to_string())
}

pub fn handles(method: &str) -> bool {
    method.starts_with("Extensions::")
}

pub fn handles_network(method: &str) -> bool {
    matches!(
        method,
        "Extensions::search" | "Extensions::fetch" | "Extensions::versions" | "Extensions::install"
    )
}

pub async fn dispatch_network(
    fs_rpc: Arc<FsRpc>,
    http: reqwest::Client,
    method: String,
    params: Value,
) -> Result<Value> {
    let workspace = fs_rpc.path("/workspace")?;
    match method.as_str() {
        "Extensions::search" => search(&workspace, &http, &params).await,
        "Extensions::fetch" => fetch(&http, &params).await,
        "Extensions::versions" => versions(&http, extension_id(&params)).await,
        "Extensions::install" => install(workspace, http, params).await,
        _ => bail!("unknown extension network method: {method}"),
    }
}

pub fn dispatch(fs_rpc: &FsRpc, method: &str, params: &Value) -> Result<Value> {
    let root = fs_rpc.path("/workspace")?;
    let host = ExtensionHost::new(root)?;
    match method {
        "Extensions::list" => host.list(),
        "Extensions::contributions" => host.contributions(),
        "Extensions::runtime_call" => host.runtime_call(params),
        "Extensions::uninstall" => {
            with_extension_write_lock(|| host.uninstall(extension_id(params)))
        }
        "Extensions::install_dev" => with_extension_write_lock(|| host.install_dev(params)),
        "Extensions::rebuild_dev" => {
            with_extension_write_lock(|| host.rebuild_dev(extension_id(params)))
        }
        _ => bail!("unknown extension method: {method}"),
    }
}

fn with_extension_write_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = EXTENSION_LIFECYCLE_LOCK
        .write()
        .map_err(|_| anyhow!("extension write lock poisoned"))?;
    operation()
}

fn runtime_worker(
    workspace: &Path,
    extension_id: &str,
    runtime: PathBuf,
) -> Result<Arc<Mutex<RuntimeWorker>>> {
    let key = RuntimeWorkerKey {
        workspace: workspace.to_path_buf(),
        extension_id: extension_id.to_string(),
    };
    let mut workers = RUNTIME_WORKERS
        .lock()
        .map_err(|_| anyhow!("extension runtime worker map poisoned"))?;
    if let Some(worker) = workers.get(&key) {
        return Ok(worker.clone());
    }
    let worker = Arc::new(Mutex::new(RuntimeWorker::start(
        runtime,
        workspace.to_path_buf(),
    )?));
    workers.insert(key, worker.clone());
    Ok(worker)
}

fn stop_runtime_worker(workspace: &Path, extension_id: &str) -> Result<()> {
    let key = RuntimeWorkerKey {
        workspace: workspace.to_path_buf(),
        extension_id: extension_id.to_string(),
    };
    let worker = RUNTIME_WORKERS
        .lock()
        .map_err(|_| anyhow!("extension runtime worker map poisoned"))?
        .remove(&key);
    if let Some(worker) = worker {
        worker
            .lock()
            .map_err(|_| anyhow!("extension runtime worker poisoned"))?
            .stop();
    }
    Ok(())
}

pub fn shutdown_runtime_workers() -> Result<()> {
    let _lifecycle_guard = EXTENSION_LIFECYCLE_LOCK
        .write()
        .map_err(|_| anyhow!("extension lifecycle lock poisoned"))?;
    let workers = {
        let mut workers = RUNTIME_WORKERS
            .lock()
            .map_err(|_| anyhow!("extension runtime worker map poisoned"))?;
        workers
            .drain()
            .map(|(_, worker)| worker)
            .collect::<Vec<_>>()
    };
    for worker in workers {
        worker
            .lock()
            .map_err(|_| anyhow!("extension runtime worker poisoned"))?
            .stop();
    }
    Ok(())
}

async fn fetch(http: &reqwest::Client, params: &Value) -> Result<Value> {
    let mut query = vec![
        ("max_schema_version", "100".to_string()),
        ("min_schema_version", "0".to_string()),
        ("filter", text(params, "query").to_string()),
    ];
    if let Some(provides) = params.get("provides").and_then(Value::as_array) {
        query.push((
            "provides",
            provides
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    get_json(http, &format!("{ZED_API}/extensions"), &query).await
}

async fn versions(http: &reqwest::Client, id: &str) -> Result<Value> {
    validate_id(id)?;
    get_json(
        http,
        &format!("{ZED_API}/extensions/{id}"),
        &[
            ("max_schema_version", "100".to_string()),
            ("min_schema_version", "0".to_string()),
        ],
    )
    .await
}

async fn search(workspace: &Path, http: &reqwest::Client, params: &Value) -> Result<Value> {
    let query = text(params, "query");
    let limit = params
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(40) as usize;
    let body = fetch(http, params).await?;
    let installed = ExtensionHost::new(workspace.to_path_buf())?.index();
    let extensions = body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .map(|item| {
            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            json!({
                "id": id,
                "name": item.get("name").and_then(Value::as_str).unwrap_or(id),
                "version": item.get("version").and_then(Value::as_str).unwrap_or_default(),
                "description": item.get("description").and_then(Value::as_str).unwrap_or_default(),
                "provides": item.get("provides").cloned().unwrap_or_else(|| json!([])),
                "download_count": item.get("download_count").and_then(Value::as_u64).unwrap_or_default(),
                "installed": installed.contains_key(id) || workspace.join(".zed/extensions").join(id).is_dir(),
                "repository": item.get("repository").and_then(Value::as_str).unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"extensions": extensions, "query": query}))
}

async fn install(workspace: PathBuf, http: reqwest::Client, params: Value) -> Result<Value> {
    let id = extension_id(&params).to_string();
    validate_id(&id)?;
    let requested = params
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
        .map(ToOwned::to_owned);
    let metadata = versions(&http, &id)
        .await
        .unwrap_or_else(|_| json!({"data": []}));
    let candidates = metadata
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected = requested
        .as_ref()
        .and_then(|version| {
            candidates
                .iter()
                .find(|candidate| candidate.get("version").and_then(Value::as_str) == Some(version))
        })
        .or_else(|| {
            candidates.iter().max_by_key(|candidate| {
                (
                    candidate
                        .get("published_at")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    candidate
                        .get("version")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            })
        });
    let version = requested.or_else(|| {
        selected
            .and_then(|value| value.get("version"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    let download = version.as_ref().map_or_else(
        || format!("{ZED_API}/extensions/{id}/download"),
        |version| format!("{ZED_API}/extensions/{id}/{version}/download"),
    );
    let response = http
        .get(download)
        .query(&[("max_schema_version", "100"), ("min_schema_version", "0")])
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "download HTTP {}: {}",
            response.status(),
            response.text().await?
        );
    }
    let archive = response.bytes().await?.to_vec();
    let name = selected
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let description = selected
        .and_then(|value| value.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let provides = selected
        .and_then(|value| value.get("provides"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    tokio::task::spawn_blocking(move || {
        install_archive(
            workspace,
            id,
            name,
            version.unwrap_or_default(),
            description,
            provides,
            archive,
        )
    })
    .await?
}

fn install_archive(
    workspace: PathBuf,
    id: String,
    name: String,
    version: String,
    description: String,
    provides: Value,
    archive: Vec<u8>,
) -> Result<Value> {
    let _guard = EXTENSION_LIFECYCLE_LOCK
        .write()
        .map_err(|_| anyhow!("extension install lock poisoned"))?;
    let host = ExtensionHost::new(workspace)?;
    stop_runtime_worker(&host.workspace, &id)?;
    let staging = host.directory.join(format!(".staging-{id}"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let decoder = flate2::read::GzDecoder::new(archive.as_slice());
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(&staging)
        .context("extracting extension archive")?;
    let children = fs::read_dir(&staging)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != "__MACOSX")
        .collect::<Vec<_>>();
    let source = if children.len() == 1 && children[0].file_type()?.is_dir() {
        children[0].path()
    } else {
        staging.clone()
    };
    let target = host.directory.join(&id);
    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    if source == staging {
        fs::rename(&staging, &target)?;
    } else {
        fs::rename(&source, &target)?;
        fs::remove_dir_all(&staging).ok();
    }
    let mut index = host.index();
    index.insert(
        id.clone(),
        json!({
            "name": name,
            "version": version,
            "description": description,
            "provides": provides,
        }),
    );
    host.write_index(&index)?;
    Ok(json!({
        "ok": true,
        "id": id,
        "name": name,
        "version": version,
        "path": target,
    }))
}

async fn get_json(http: &reqwest::Client, url: &str, query: &[(&str, String)]) -> Result<Value> {
    let response = http.get(url).query(query).send().await?;
    if !response.status().is_success() {
        bail!("HTTP {}: {}", response.status(), response.text().await?);
    }
    Ok(serde_json::from_slice(&response.bytes().await?)?)
}

fn text<'a>(params: &'a Value, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).unwrap_or_default()
}

struct ExtensionHost {
    workspace: PathBuf,
    directory: PathBuf,
    index_path: PathBuf,
}

impl ExtensionHost {
    fn new(workspace: PathBuf) -> Result<Self> {
        let workspace = workspace.canonicalize().context("invalid workspace path")?;
        let directory = workspace.join(".zed/extensions");
        fs::create_dir_all(&directory)?;
        let index_path = directory.join("index.json");
        if !index_path.exists() {
            fs::write(&index_path, "{}\n")?;
        }
        Ok(Self {
            workspace,
            directory,
            index_path,
        })
    }

    fn index(&self) -> BTreeMap<String, Value> {
        fs::read_to_string(&self.index_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_index(&self, index: &BTreeMap<String, Value>) -> Result<()> {
        let parent = self
            .index_path
            .parent()
            .context("extension index path has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(index)?)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&self.index_path)
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn installed(&self) -> Result<Vec<Value>> {
        let index = self.index();
        let mut ids = index.keys().cloned().collect::<Vec<_>>();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let id = entry.file_name().to_string_lossy().to_string();
                if !id.starts_with('.') && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids
            .into_iter()
            .map(|id| {
                let metadata = index.get(&id).and_then(Value::as_object);
                let manifest = fs::read_to_string(self.directory.join(&id).join("extension.toml"))
                    .unwrap_or_default();
                let parsed = manifest.parse::<toml::Value>().ok();
                let field = |name: &str| {
                    metadata
                        .and_then(|value| value.get(name))
                        .cloned()
                        .or_else(|| {
                            parsed
                                .as_ref()
                                .and_then(|value| value.get(name))
                                .and_then(toml::Value::as_str)
                                .map(|value| json!(value))
                        })
                        .unwrap_or(Value::Null)
                };
                json!({
                    "id": id,
                    "name": field("name").as_str().unwrap_or(&id),
                    "version": field("version").as_str().unwrap_or_default(),
                    "description": field("description").as_str().unwrap_or_default(),
                    "provides": metadata.and_then(|value| value.get("provides")).cloned().unwrap_or_else(|| json!([])),
                    "installed": true,
                    "manifest_toml": manifest,
                    "dev": metadata.and_then(|value| value.get("dev")).and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect())
    }

    fn list(&self) -> Result<Value> {
        Ok(json!({
            "extensions": self.installed()?,
            "dir": self.directory,
        }))
    }

    fn contributions(&self) -> Result<Value> {
        let mut extensions = Vec::new();
        for installed in self.installed()? {
            let id = installed["id"].as_str().unwrap_or_default();
            let directory = self.directory.join(id);
            let manifest = fs::read_to_string(directory.join("extension.toml"))
                .ok()
                .and_then(|text| text.parse::<toml::Value>().ok());
            let themes = manifest_text_assets(&directory, manifest.as_ref(), "themes")?;
            let icon_themes = manifest_text_assets(&directory, manifest.as_ref(), "icon_themes")?;
            let icon_assets = text_assets(&directory, ".", "svg")?;
            let languages = language_assets(&directory, manifest.as_ref())?;
            let snippets = snippet_assets(&directory, manifest.as_ref())?;
            let grammars = grammar_assets(&directory, manifest.as_ref())?;
            let debug_adapters = debug_adapter_assets(&directory, manifest.as_ref())?;
            let language_servers =
                table_entries(manifest.as_ref(), "language_servers", |id, entry| {
                    let languages = entry
                        .get("languages")
                        .and_then(toml::Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(toml::Value::as_str)
                                .collect::<Vec<_>>()
                        })
                        .filter(|values| !values.is_empty())
                        .unwrap_or_else(|| {
                            entry
                                .get("language")
                                .and_then(toml::Value::as_str)
                                .into_iter()
                                .collect()
                        });
                    json!({
                        "id": id,
                        "languages": languages,
                        "language_ids": toml_json(entry.get("language_ids")),
                        "code_action_kinds": toml_json(entry.get("code_action_kinds")),
                    })
                });
            extensions.push(json!({
                "id": id,
                "themes": themes,
                "icon_themes": icon_themes,
                "icon_assets": icon_assets,
                "languages": languages,
                "snippets": snippets,
                "grammars": grammars,
                "language_servers": language_servers,
                "context_servers": table_keys(manifest.as_ref(), "context_servers"),
                "slash_commands": table_entries(manifest.as_ref(), "slash_commands", |name, entry| json!({
                    "name": name,
                    "description": entry.get("description").and_then(toml::Value::as_str).unwrap_or_default(),
                    "requires_argument": entry.get("requires_argument").and_then(toml::Value::as_bool).unwrap_or(false),
                })),
                "debug_adapters": debug_adapters,
                "debug_locators": table_keys(manifest.as_ref(), "debug_locators"),
                "language_model_providers": table_entries(manifest.as_ref(), "language_model_providers", |provider_id, entry| json!({
                    "id": provider_id,
                    "name": entry.get("name").and_then(toml::Value::as_str).unwrap_or(provider_id),
                    "icon": entry.get("icon").and_then(toml::Value::as_str),
                })),
            }));
        }
        Ok(json!({"extensions": extensions}))
    }

    fn runtime_call(&self, params: &Value) -> Result<Value> {
        let _lifecycle_guard = EXTENSION_LIFECYCLE_LOCK
            .read()
            .map_err(|_| anyhow!("extension lifecycle lock poisoned"))?;
        let id = extension_id(params);
        validate_id(id)?;
        let extension_directory = self.directory.join(id);
        if !extension_directory.join("extension.toml").is_file()
            || !extension_directory.join("extension.wasm").is_file()
        {
            return Ok(json!({"ok": false, "error": "extension runtime is not installed"}));
        }
        let runtime = resolve_extension_runtime(&self.workspace);
        let Some(runtime) = runtime.filter(|path| path.is_file()) else {
            return Ok(json!({"ok": false, "error": "extension runtime is not built"}));
        };
        let mut request = params.clone();
        let request_object = request
            .as_object_mut()
            .ok_or_else(|| anyhow!("extension runtime params must be an object"))?;
        request_object.insert(
            "extension_dir".to_string(),
            json!(extension_directory.to_string_lossy()),
        );
        let worktree_root = runtime_worktree_root(request_object, &self.workspace)?;
        request_object.insert(
            "worktree_root".to_string(),
            json!(worktree_root.to_string_lossy()),
        );
        let worker = runtime_worker(&self.workspace, id, runtime)?;
        worker
            .lock()
            .map_err(|_| anyhow!("extension runtime worker poisoned"))?
            .call(&request)
    }

    fn uninstall(&self, id: &str) -> Result<Value> {
        validate_id(id)?;
        stop_runtime_worker(&self.workspace, id)?;
        let target = self.directory.join(id);
        if target.exists() {
            fs::remove_dir_all(target)?;
        }
        let mut index = self.index();
        index.remove(id);
        self.write_index(&index)?;
        Ok(json!({"ok": true, "id": id}))
    }

    fn install_dev(&self, params: &Value) -> Result<Value> {
        let raw = params
            .get("path")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.workspace.join(raw)
        };
        let source = source.canonicalize()?;
        if !source.starts_with(&self.workspace) {
            bail!("extension path is outside the workspace");
        }
        let manifest_text = fs::read_to_string(source.join("extension.toml"))?;
        let manifest = manifest_text.parse::<toml::Value>()?;
        let id = manifest
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| anyhow!("extension.toml has no id"))?;
        validate_id(id)?;
        stop_runtime_worker(&self.workspace, id)?;
        let target = self.directory.join(id);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        copy_tree(&source, &target)?;
        let mut index = self.index();
        index.insert(
            id.to_string(),
            json!({
                "name": manifest.get("name").and_then(toml::Value::as_str).unwrap_or(id),
                "version": manifest.get("version").and_then(toml::Value::as_str).unwrap_or_default(),
                "description": manifest.get("description").and_then(toml::Value::as_str).unwrap_or_default(),
                "provides": [],
                "dev": true,
                "dev_source": source,
            }),
        );
        self.write_index(&index)?;
        Ok(json!({"ok": true, "id": id, "path": target}))
    }

    fn rebuild_dev(&self, id: &str) -> Result<Value> {
        validate_id(id)?;
        let index = self.index();
        let Some(source) = index
            .get(id)
            .and_then(|metadata| metadata.get("dev_source"))
            .and_then(Value::as_str)
        else {
            return Ok(json!({"ok": false, "error": "development extension is not installed"}));
        };
        self.install_dev(&json!({"path": source}))
    }
}

fn extension_id(params: &Value) -> &str {
    params
        .get("id")
        .or_else(|| params.get("extension_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || Path::new(id).file_name().and_then(|name| name.to_str()) != Some(id) {
        bail!("invalid extension id");
    }
    Ok(())
}

fn text_assets(root: &Path, subdirectory: &str, extension: &str) -> Result<Vec<Value>> {
    let directory = root.join(subdirectory);
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    walkdir::WalkDir::new(directory)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some(extension)
        })
        .map(|entry| {
            Ok(json!({
                "relative_path": entry.path().strip_prefix(root)?.to_string_lossy(),
                "content": fs::read_to_string(entry.path())?,
            }))
        })
        .collect()
}

fn manifest_text_assets(
    root: &Path,
    manifest: Option<&toml::Value>,
    key: &str,
) -> Result<Vec<Value>> {
    manifest_paths(manifest, key)
        .map(|relative_path| {
            let path = checked_extension_path(root, &relative_path)?;
            Ok(json!({
                "relative_path": relative_path,
                "content": fs::read_to_string(path)?,
            }))
        })
        .collect()
}

fn language_assets(root: &Path, manifest: Option<&toml::Value>) -> Result<Vec<Value>> {
    manifest_paths(manifest, "languages")
        .map(|relative_path| {
            let directory = checked_extension_path(root, &relative_path)?;
            let config = directory.join("config.toml");
            let relative_config = PathBuf::from(&relative_path).join("config.toml");
            let tasks = read_optional_text(&directory.join("tasks.json"))?;
            let semantic_token_rules =
                read_optional_text(&directory.join("semantic_token_rules.json"))?;
            let queries = fs::read_dir(&directory)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("scm")
                })
                .map(|entry| {
                    Ok((
                        entry.file_name().to_string_lossy().to_string(),
                        Value::String(fs::read_to_string(entry.path())?),
                    ))
                })
                .collect::<Result<Map<_, _>>>()?;
            Ok(json!({
                "relative_path": relative_config.to_string_lossy(),
                "config": fs::read_to_string(config)?,
                "queries": queries,
                "tasks": tasks,
                "semantic_token_rules": semantic_token_rules,
            }))
        })
        .collect()
}

fn manifest_paths<'a>(
    manifest: Option<&'a toml::Value>,
    key: &str,
) -> impl Iterator<Item = String> + 'a {
    manifest
        .and_then(|value| value.get(key))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(ToOwned::to_owned)
}

fn checked_extension_path(root: &Path, relative_path: &str) -> Result<PathBuf> {
    let path = root.join(relative_path).canonicalize()?;
    let root = root.canonicalize()?;
    if !path.starts_with(&root) {
        bail!("extension asset path escapes extension directory");
    }
    Ok(path)
}

fn debug_adapter_assets(root: &Path, manifest: Option<&toml::Value>) -> Result<Vec<Value>> {
    let Some(adapters) = manifest
        .and_then(|value| value.get("debug_adapters"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(Vec::new());
    };

    adapters
        .iter()
        .filter_map(|(adapter_id, value)| Some((adapter_id, value.as_table()?)))
        .map(|(adapter_id, entry)| {
            let relative_path = entry
                .get("schema_path")
                .and_then(toml::Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from("debug_adapter_schemas").join(format!("{adapter_id}.json"))
                });
            let schema_path = root.join(relative_path);
            let schema = match fs::read_to_string(&schema_path) {
                Ok(content) => serde_json::from_str(&content)
                    .with_context(|| format!("parsing debug adapter schema {schema_path:?}"))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading debug adapter schema {schema_path:?}"));
                }
            };
            Ok(json!({"id": adapter_id, "schema": schema}))
        })
        .collect()
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading extension asset {path:?}")),
    }
}

fn resolve_extension_runtime(workspace: &Path) -> Option<PathBuf> {
    std::env::var_os("ZED_EXTENSION_RUNTIME")
        .map(PathBuf::from)
        .or_else(|| {
            [
                workspace
                    .parent()?
                    .join("target-native/release/zed-extension-runtime"),
                workspace
                    .parent()?
                    .join("zed-repo/target/release/zed-extension-runtime"),
            ]
            .into_iter()
            .find(|path| path.is_file())
        })
}

fn runtime_worktree_root(params: &Map<String, Value>, fallback: &Path) -> Result<PathBuf> {
    let worktree_root = params
        .get("worktree_root")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
        .canonicalize()
        .context("invalid extension worktree root")?;
    if !worktree_root.is_dir() {
        bail!("extension worktree root is not a directory");
    }
    Ok(worktree_root)
}

fn grammar_assets(root: &Path, manifest: Option<&toml::Value>) -> Result<Vec<Value>> {
    table_keys(manifest, "grammars")
        .into_iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .map(|id| {
            let bytes = fs::read(root.join("grammars").join(format!("{id}.wasm")))?;
            Ok(json!({"id": id, "content_base64": BASE64.encode(bytes)}))
        })
        .collect()
}

fn snippet_assets(root: &Path, manifest: Option<&toml::Value>) -> Result<Vec<Value>> {
    let Some(value) = manifest.and_then(|value| value.get("snippets")) else {
        return Ok(Vec::new());
    };
    let paths = value.as_str().into_iter().map(ToOwned::to_owned).chain(
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(ToOwned::to_owned),
    );
    paths
        .map(|relative| {
            let path = root.join(&relative).canonicalize()?;
            if !path.starts_with(root) {
                bail!("snippet path escapes extension");
            }
            Ok(json!({"relative_path": relative, "content": fs::read_to_string(path)?}))
        })
        .collect()
}

fn table_keys(manifest: Option<&toml::Value>, key: &str) -> Vec<Value> {
    manifest
        .and_then(|value| value.get(key))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|table| table.keys())
        .map(|key| json!(key))
        .collect()
}

fn table_entries(
    manifest: Option<&toml::Value>,
    key: &str,
    convert: impl Fn(&str, &toml::value::Table) -> Value,
) -> Vec<Value> {
    manifest
        .and_then(|value| value.get(key))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|table| table.iter())
        .filter_map(|(id, value)| Some(convert(id, value.as_table()?)))
        .collect()
}

fn toml_json(value: Option<&toml::Value>) -> Value {
    value
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or_else(|| json!({}))
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(source) {
        let entry = entry?;
        let destination = target.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(destination)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable_script(directory: &Path, name: &str, contents: &str) -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join(name);
        fs::write(&path, contents)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    #[test]
    fn language_assets_include_editor_contributions() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let language = temporary.path().join("languages/example");
        fs::create_dir_all(&language)?;
        fs::write(
            language.join("config.toml"),
            "name = \"Example\"\ngrammar = \"example\"\n",
        )?;
        fs::write(language.join("highlights.scm"), "(identifier) @variable")?;
        fs::write(language.join("tasks.json"), "[{\"label\":\"Run\"}]")?;
        fs::write(
            language.join("semantic_token_rules.json"),
            "[{\"token_type\":\"variable\",\"style\":[\"italic\"]}]",
        )?;
        let manifest = r#"languages = ["languages/example"]"#.parse::<toml::Value>()?;

        let assets = language_assets(temporary.path(), Some(&manifest))?;
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["tasks"], "[{\"label\":\"Run\"}]");
        assert_eq!(
            assets[0]["semantic_token_rules"],
            "[{\"token_type\":\"variable\",\"style\":[\"italic\"]}]"
        );
        assert_eq!(
            assets[0]["queries"]["highlights.scm"],
            "(identifier) @variable"
        );
        Ok(())
    }

    #[test]
    fn text_assets_follow_manifest_paths() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        fs::create_dir_all(temporary.path().join("custom"))?;
        fs::write(
            temporary.path().join("custom/theme.json"),
            "{\"name\":\"Example\"}",
        )?;
        let manifest = r#"themes = ["custom/theme.json"]"#.parse::<toml::Value>()?;

        let assets = manifest_text_assets(temporary.path(), Some(&manifest), "themes")?;
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["relative_path"], "custom/theme.json");
        assert_eq!(assets[0]["content"], "{\"name\":\"Example\"}");
        Ok(())
    }

    #[test]
    fn debug_adapter_assets_include_schema() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        fs::create_dir_all(temporary.path().join("schemas"))?;
        fs::write(
            temporary.path().join("schemas/example.json"),
            "{\"type\":\"object\",\"required\":[\"program\"]}",
        )?;
        let manifest = r#"
            [debug_adapters.example]
            schema_path = "schemas/example.json"
        "#
        .parse::<toml::Value>()?;

        let assets = debug_adapter_assets(temporary.path(), Some(&manifest))?;
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["id"], "example");
        assert_eq!(assets[0]["schema"]["type"], "object");
        assert_eq!(assets[0]["schema"]["required"][0], "program");
        Ok(())
    }

    #[test]
    fn runtime_uses_requested_worktree_root() -> Result<()> {
        let fallback = tempfile::tempdir()?;
        let requested = tempfile::tempdir()?;
        let params = serde_json::from_value::<Map<String, Value>>(json!({
            "worktree_root": requested.path()
        }))?;

        assert_eq!(
            runtime_worktree_root(&params, fallback.path())?,
            requested.path().canonicalize()?
        );
        assert_eq!(
            runtime_worktree_root(&Map::new(), fallback.path())?,
            fallback.path().canonicalize()?
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn runtime_worker_preserves_process_state_across_calls() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let runtime = executable_script(
            temporary.path(),
            "persistent-runtime",
            r#"#!/bin/sh
count=0
while IFS= read -r request; do
    count=$((count + 1))
    printf 'ZED_EXTENSION_RESPONSE {"ok":true,"result":{"count":%s,"pid":%s}}\n' "$count" "$$"
done
"#,
        )?;
        let mut worker = RuntimeWorker::start(runtime, temporary.path().to_path_buf())?;

        let first = worker.call(&json!({"method": "first"}))?;
        let second = worker.call(&json!({"method": "second"}))?;

        assert_eq!(first["result"]["count"], 1);
        assert_eq!(second["result"]["count"], 2);
        assert_eq!(first["result"]["pid"], second["result"]["pid"]);
        worker.stop();
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn runtime_worker_restarts_after_process_exit() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let runtime = executable_script(
            temporary.path(),
            "restarting-runtime",
            r#"#!/bin/sh
generation_file="$0.generation"
generation=0
if test -f "$generation_file"; then
    generation=$(cat "$generation_file")
fi
generation=$((generation + 1))
printf '%s\n' "$generation" > "$generation_file"
IFS= read -r request || exit 0
printf 'ZED_EXTENSION_RESPONSE {"ok":true,"result":{"generation":%s}}\n' "$generation"
exit 1
"#,
        )?;
        let mut worker = RuntimeWorker::start(runtime, temporary.path().to_path_buf())?;

        let first = worker.call(&json!({"method": "first"}))?;
        let second = worker.call(&json!({"method": "second"}))?;

        assert_eq!(first["result"]["generation"], 1);
        assert_eq!(second["result"]["generation"], 2);
        worker.stop();
        Ok(())
    }
}
