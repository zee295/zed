use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};

use crate::fs_rpc::FsRpc;

const ZED_API: &str = "https://api.zed.dev";
static EXTENSION_WRITE_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = EXTENSION_WRITE_LOCK
        .lock()
        .map_err(|_| anyhow!("extension write lock poisoned"))?;
    operation()
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
    let _guard = EXTENSION_WRITE_LOCK
        .lock()
        .map_err(|_| anyhow!("extension install lock poisoned"))?;
    let host = ExtensionHost::new(workspace)?;
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
            let themes = text_assets(&directory, "themes", "json")?;
            let icon_themes = text_assets(&directory, "icon_themes", "json")?;
            let icon_assets = text_assets(&directory, ".", "svg")?;
            let languages = language_assets(&directory)?;
            let snippets = snippet_assets(&directory, manifest.as_ref())?;
            let grammars = grammar_assets(&directory, manifest.as_ref())?;
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
                "debug_adapters": table_entries(manifest.as_ref(), "debug_adapters", |adapter_id, _| json!({
                    "id": adapter_id,
                    "schema": {}
                })),
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
        let id = extension_id(params);
        validate_id(id)?;
        let extension_directory = self.directory.join(id);
        if !extension_directory.join("extension.toml").is_file()
            || !extension_directory.join("extension.wasm").is_file()
        {
            return Ok(json!({"ok": false, "error": "extension runtime is not installed"}));
        }
        let runtime = std::env::var_os("ZED_EXTENSION_RUNTIME")
            .map(PathBuf::from)
            .or_else(|| {
                [
                    self.workspace
                        .parent()?
                        .join("target-native/release/zed-extension-runtime"),
                    self.workspace
                        .parent()?
                        .join("zed-repo/target/release/zed-extension-runtime"),
                ]
                .into_iter()
                .find(|path| path.is_file())
            });
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
        request_object.insert(
            "worktree_root".to_string(),
            json!(self.workspace.to_string_lossy()),
        );
        let mut child = Command::new(runtime)
            .current_dir(&self.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("extension runtime stdin")?
            .write_all(serde_json::to_string(&request)?.as_bytes())?;
        let output = child.wait_with_output()?;
        let line = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .next_back()
            .map(ToOwned::to_owned);
        match line {
            Some(line) => Ok(serde_json::from_str(&line)?),
            None => Ok(json!({
                "ok": false,
                "error": format!("extension runtime returned no result: {}", String::from_utf8_lossy(&output.stderr).trim())
            })),
        }
    }

    fn uninstall(&self, id: &str) -> Result<Value> {
        validate_id(id)?;
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

fn language_assets(root: &Path) -> Result<Vec<Value>> {
    let directory = root.join("languages");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join("config.toml").is_file())
        .map(|entry| {
            let directory = entry.path();
            let config = directory.join("config.toml");
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
                "relative_path": config.strip_prefix(root)?.to_string_lossy(),
                "config": fs::read_to_string(config)?,
                "queries": queries,
            }))
        })
        .collect()
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
