use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Write as _,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

const VIRTUAL_ROOT: &str = "/workspace";

#[derive(Clone)]
pub struct FsRpc {
    root: Arc<PathBuf>,
    restrict_paths: bool,
    trash_root: Arc<PathBuf>,
    next_trash_id: Arc<AtomicU64>,
    trash_entries: Arc<Mutex<HashMap<u64, (PathBuf, PathBuf)>>>,
}

impl FsRpc {
    pub fn new(root: PathBuf, restrict_paths: bool) -> Result<Self> {
        Ok(Self {
            root: Arc::new(root.canonicalize().context("canonicalizing project root")?),
            restrict_paths,
            trash_root: Arc::new(tempfile::tempdir()?.keep()),
            next_trash_id: Arc::new(AtomicU64::new(1)),
            trash_entries: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn handles(method: &str) -> bool {
        method.starts_with("Fs::") && method != "Fs::watch"
    }

    pub fn dispatch(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "Fs::create_dir" => self.create_dir(params),
            "Fs::create_symlink" => self.create_symlink(params),
            "Fs::create_file" => self.create_file(params),
            "Fs::create_file_with" => self.create_file_with(params),
            "Fs::copy_file" => self.copy_file(params),
            "Fs::rename" => self.rename(params),
            "Fs::remove_dir" => self.remove_dir(params),
            "Fs::remove_file" => self.remove_file(params),
            "Fs::trash" => self.trash(params),
            "Fs::restore" => self.restore(params),
            "Fs::open_handle" => self.open_handle(params),
            "Fs::open_sync" | "Fs::load_bytes" => self.load_bytes(params),
            "Fs::load" => self.load(params),
            "Fs::atomic_write" => self.atomic_write(params),
            "Fs::save" => self.save(params),
            "Fs::write" => self.write(params),
            "Fs::canonicalize" => self.canonicalize(params),
            "Fs::is_file" => self.is_file(params),
            "Fs::is_dir" => self.is_dir(params),
            "Fs::metadata" => self.metadata(params),
            "Fs::read_link" => self.read_link(params),
            "Fs::read_dir" => self.read_dir(params),
            "Fs::read_dir_tree" => self.read_dir_tree(params),
            "Fs::is_case_sensitive" => Ok(Value::Bool(false)),
            "Fs::git_init" => self.git_init(params),
            "Fs::git_clone" => self.git_clone(params),
            "Fs::git_config" => self.git_config(params),
            _ => bail!("unknown filesystem method: {method}"),
        }
    }

    pub fn path(&self, requested: &str) -> Result<PathBuf> {
        let raw = if requested.trim().is_empty() {
            "."
        } else {
            requested.trim()
        };
        let (candidate, project_relative) = if raw == VIRTUAL_ROOT {
            (self.root.as_ref().clone(), true)
        } else if raw.starts_with(&format!("{VIRTUAL_ROOT}/")) {
            let relative = raw.trim_start_matches(VIRTUAL_ROOT).trim_start_matches('/');
            (self.root.join(relative), true)
        } else {
            let path = Path::new(raw);
            if path.is_absolute() {
                (path.to_path_buf(), false)
            } else {
                (self.root.join(path), true)
            }
        };

        if project_relative && has_parent_component(&candidate.strip_prefix(&*self.root)?) {
            bail!("path escapes project-relative path");
        }
        if self.restrict_paths {
            let resolved = canonicalize_existing_parent(&candidate)?;
            if !resolved.starts_with(&*self.root) {
                bail!("path escapes configured root");
            }
            return Ok(resolved);
        }
        Ok(candidate)
    }

    pub fn virtualize(&self, path: &Path) -> String {
        path.strip_prefix(&*self.root).map_or_else(
            |_| path.display().to_string(),
            |relative| {
                if relative.as_os_str().is_empty() {
                    VIRTUAL_ROOT.to_string()
                } else {
                    format!("{VIRTUAL_ROOT}/{}", relative.to_string_lossy())
                }
            },
        )
    }

    fn requested<'a>(&self, params: &'a Value, key: &str) -> &'a str {
        params.get(key).and_then(Value::as_str).unwrap_or(".")
    }

    fn create_dir(&self, params: &Value) -> Result<Value> {
        fs::create_dir_all(self.path(self.requested(params, "path"))?)?;
        Ok(Value::Null)
    }

    fn create_symlink(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        ensure_parent(&path)?;
        if path.symlink_metadata().is_ok() {
            remove_path(&path)?;
        }
        std::os::unix::fs::symlink(
            params.get("target").and_then(Value::as_str).unwrap_or(""),
            path,
        )?;
        Ok(Value::Null)
    }

    fn create_file(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if path.exists() {
            if bool_param(params, "ignore_if_exists") {
                return Ok(Value::Null);
            }
            if !bool_param(params, "overwrite") {
                bail!("file already exists: {}", path.display());
            }
        }
        ensure_parent(&path)?;
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(bool_param(params, "overwrite"))
            .open(path)?;
        Ok(Value::Null)
    }

    fn create_file_with(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        ensure_parent(&path)?;
        fs::write(path, decode_content(params)?)?;
        Ok(Value::Null)
    }

    fn copy_file(&self, params: &Value) -> Result<Value> {
        let source = self.path(self.requested(params, "source"))?;
        let target = self.path(self.requested(params, "target"))?;
        if target.exists() {
            if bool_param(params, "ignore_if_exists") {
                return Ok(Value::Null);
            }
            if !bool_param(params, "overwrite") {
                bail!("target exists: {}", target.display());
            }
        }
        ensure_parent(&target)?;
        fs::copy(source, target)?;
        Ok(Value::Null)
    }

    fn rename(&self, params: &Value) -> Result<Value> {
        let source = self.path(self.requested(params, "source"))?;
        let target = self.path(self.requested(params, "target"))?;
        if target.exists() {
            if bool_param(params, "ignore_if_exists") {
                return Ok(Value::Null);
            }
            if !bool_param(params, "overwrite") {
                bail!("target exists: {}", target.display());
            }
            remove_path(&target)?;
        }
        if bool_param(params, "create_parents") {
            ensure_parent(&target)?;
        }
        fs::rename(source, target)?;
        Ok(Value::Null)
    }

    fn remove_dir(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if !path.exists() {
            if bool_param(params, "ignore_if_not_exists") {
                return Ok(Value::Null);
            }
            bail!("path does not exist: {}", path.display());
        }
        if bool_param(params, "recursive") {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_dir(path)?;
        }
        Ok(Value::Null)
    }

    fn remove_file(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if !path.exists() {
            if bool_param(params, "ignore_if_not_exists") {
                return Ok(Value::Null);
            }
            bail!("path does not exist: {}", path.display());
        }
        remove_path(&path)?;
        Ok(Value::Null)
    }

    fn trash(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if path.symlink_metadata().is_err() {
            bail!("path does not exist: {}", path.display());
        }
        let trash_id = self.next_trash_id.fetch_add(1, Ordering::Relaxed);
        let trashed_path = self.trash_root.join(trash_id.to_string());
        fs::rename(&path, &trashed_path)?;
        self.trash_entries
            .lock()
            .map_err(|_| anyhow!("trash lock poisoned"))?
            .insert(trash_id, (path, trashed_path));
        Ok(json!(trash_id))
    }

    fn restore(&self, params: &Value) -> Result<Value> {
        let trash_id = params.get("trash_id").and_then(Value::as_u64).unwrap_or(0);
        let mut entries = self
            .trash_entries
            .lock()
            .map_err(|_| anyhow!("trash lock poisoned"))?;
        let Some((original, trashed)) = entries.get(&trash_id).cloned() else {
            return Ok(json!({"ok": false, "error": "already_restored"}));
        };
        if trashed.symlink_metadata().is_err() {
            return Ok(json!({
                "ok": false,
                "error": "not_found",
                "path": self.virtualize(&original)
            }));
        }
        if original.symlink_metadata().is_ok() {
            return Ok(json!({
                "ok": false,
                "error": "collision",
                "path": self.virtualize(&original)
            }));
        }
        ensure_parent(&original)?;
        fs::rename(trashed, &original)?;
        entries.remove(&trash_id);
        Ok(json!({"ok": true, "path": self.virtualize(&original)}))
    }

    fn open_handle(&self, params: &Value) -> Result<Value> {
        Ok(json!(
            self.virtualize(&self.path(self.requested(params, "path"))?)
        ))
    }

    fn load(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if !path.exists() {
            return Ok(json!(""));
        }
        if !path.is_file() {
            bail!("not a file: {}", path.display());
        }
        Ok(json!(String::from_utf8_lossy(&fs::read(path)?)))
    }

    fn load_bytes(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if !path.exists() {
            return Ok(json!(""));
        }
        if !path.is_file() {
            bail!("not a file: {}", path.display());
        }
        Ok(json!(BASE64.encode(fs::read(path)?)))
    }

    fn atomic_write(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        tracing::debug!(
            path = %path.display(),
            bytes = text_param(params).len(),
            "filesystem atomic write"
        );
        ensure_parent(&path)?;
        let parent = path.parent().context("atomic write path has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(text_param(params).as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(Value::Null)
    }

    fn save(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        ensure_parent(&path)?;
        fs::write(path, text_param(params))?;
        Ok(Value::Null)
    }

    fn write(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        ensure_parent(&path)?;
        fs::write(path, decode_content(params)?)?;
        Ok(Value::Null)
    }

    fn canonicalize(&self, params: &Value) -> Result<Value> {
        let path = canonicalize_existing_parent(&self.path(self.requested(params, "path"))?)?;
        Ok(json!(self.virtualize(&path)))
    }

    fn is_file(&self, params: &Value) -> Result<Value> {
        Ok(json!(
            self.path(self.requested(params, "path"))
                .is_ok_and(|path| path.is_file())
        ))
    }

    fn is_dir(&self, params: &Value) -> Result<Value> {
        Ok(json!(
            self.path(self.requested(params, "path"))
                .is_ok_and(|path| path.is_dir())
        ))
    }

    fn metadata(&self, params: &Value) -> Result<Value> {
        let Ok(path) = self.path(self.requested(params, "path")) else {
            return Ok(Value::Null);
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return Ok(Value::Null);
        };
        Ok(metadata_value(&metadata))
    }

    fn read_link(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        let target = fs::read_link(&path)
            .with_context(|| format!("failed to read link {}", self.virtualize(&path)))?;
        let target = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(&*self.root).join(target)
        };
        Ok(json!(self.virtualize(&target)))
    }

    fn read_dir(&self, params: &Value) -> Result<Value> {
        let path = self.path(self.requested(params, "path"))?;
        if !path.exists() {
            return Ok(json!({"entries": []}));
        }
        if !path.is_dir() {
            bail!("not a directory: {}", path.display());
        }
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|path| path.file_name().map(|name| name.to_ascii_lowercase()));
        Ok(json!({
            "entries": entries
                .iter()
                .map(|entry| self.virtualize(entry))
                .collect::<Vec<_>>()
        }))
    }

    fn read_dir_tree(&self, params: &Value) -> Result<Value> {
        const MAX_ENTRIES: usize = 50_000;

        let root = self.path(self.requested(params, "path"))?;
        if !root.exists() {
            return Ok(json!({"entries": [], "directories": {}, "metadata": {}}));
        }
        if !root.is_dir() {
            bail!("not a directory: {}", root.display());
        }

        let mut pending = VecDeque::from([root.clone()]);
        let mut root_entries = Vec::new();
        let mut directories = serde_json::Map::new();
        let mut metadata = serde_json::Map::new();
        let mut entry_count = 0;

        while let Some(directory) = pending.pop_front() {
            let Ok(read_dir) = fs::read_dir(&directory) else {
                continue;
            };
            let mut entries = read_dir.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name().to_ascii_lowercase());

            let mut directory_entries = Vec::with_capacity(entries.len());
            for entry in entries {
                if entry_count >= MAX_ENTRIES {
                    break;
                }
                entry_count += 1;
                let path = entry.path();
                let virtual_path = self.virtualize(&path);
                let Ok(entry_metadata) = entry.metadata() else {
                    continue;
                };
                metadata.insert(virtual_path.clone(), metadata_value(&entry_metadata));
                directory_entries.push(virtual_path);

                if entry_metadata.is_dir() && should_prefetch_directory(&entry) {
                    pending.push_back(path);
                }
            }

            if directory == root {
                root_entries = directory_entries;
            } else {
                directories.insert(self.virtualize(&directory), json!(directory_entries));
            }
            if entry_count >= MAX_ENTRIES {
                break;
            }
        }

        Ok(json!({
            "entries": root_entries,
            "directories": directories,
            "metadata": metadata,
        }))
    }

    fn git_init(&self, params: &Value) -> Result<Value> {
        let directory = self.path(self.requested(params, "abs_work_directory"))?;
        let branch = params
            .get("fallback_branch_name")
            .and_then(Value::as_str)
            .unwrap_or("main");
        run_git(&directory, ["init", "-b", branch])?;
        Ok(Value::Null)
    }

    fn git_clone(&self, params: &Value) -> Result<Value> {
        let directory = self.path(self.requested(params, "abs_work_directory"))?;
        let url = params.get("repo_url").and_then(Value::as_str).unwrap_or("");
        let destination = directory.to_string_lossy().into_owned();
        run_git(&self.root, ["clone", url, destination.as_str()])?;
        Ok(Value::Null)
    }

    fn git_config(&self, params: &Value) -> Result<Value> {
        let directory = self.path(self.requested(params, "abs_work_directory"))?;
        let mut arguments = vec!["config".to_string()];
        arguments.extend(
            params
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned),
        );
        run_git(&directory, arguments.iter().map(String::as_str))?;
        Ok(Value::Null)
    }
}

fn bool_param(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn text_param(params: &Value) -> &str {
    params.get("text").and_then(Value::as_str).unwrap_or("")
}

fn decode_content(params: &Value) -> Result<Vec<u8>> {
    BASE64
        .decode(params.get("content").and_then(Value::as_str).unwrap_or(""))
        .context("decoding base64 content")
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn metadata_value(metadata: &fs::Metadata) -> Value {
    let mode = metadata.permissions().mode();
    json!({
        "inode": metadata.ino(),
        "mtime_secs": metadata.mtime(),
        "mtime_nanos": metadata.mtime_nsec(),
        "is_symlink": metadata.file_type().is_symlink(),
        "is_dir": metadata.is_dir(),
        "len": metadata.len(),
        "is_fifo": metadata.file_type().is_fifo(),
        "is_executable": mode & 0o100 != 0,
        "is_writable": mode & 0o200 != 0,
    })
}

fn should_prefetch_directory(entry: &fs::DirEntry) -> bool {
    !matches!(
        entry.file_name().to_str(),
        Some(
            "node_modules"
                | ".git"
                | "vendor"
                | ".venv"
                | "venv"
                | ".next"
                | "target"
                | "dist"
                | ".cache"
                | "__pycache__"
        )
    )
}

fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn canonicalize_existing_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name"))?;
    Ok(parent.canonicalize()?.join(file_name))
}

fn run_git<'a>(directory: &Path, arguments: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_alias_is_project_relative() -> Result<()> {
        let root = tempfile::tempdir()?;
        let rpc = FsRpc::new(root.path().to_path_buf(), false)?;
        assert_eq!(
            rpc.path("/workspace/src/main.rs")?,
            root.path().canonicalize()?.join("src/main.rs")
        );
        assert_eq!(
            rpc.path("/workspace")?.to_string_lossy(),
            root.path().canonicalize()?.to_string_lossy()
        );
        assert!(rpc.path("/workspace/../outside").is_err());
        Ok(())
    }

    #[test]
    fn trash_restore_preserves_collision() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("note.txt"), "old")?;
        let rpc = FsRpc::new(root.path().to_path_buf(), false)?;
        let trash_id = rpc
            .dispatch("Fs::trash", &json!({"path": "/workspace/note.txt"}))?
            .as_u64()
            .unwrap_or_default();
        fs::write(root.path().join("note.txt"), "new")?;
        assert_eq!(
            rpc.dispatch("Fs::restore", &json!({"trash_id": trash_id}))?["error"],
            "collision"
        );
        Ok(())
    }

    #[test]
    fn concurrent_atomic_writes_do_not_share_staging_path() -> Result<()> {
        let root = tempfile::tempdir()?;
        let rpc = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let threads = (0..8)
            .map(|index| {
                let rpc = rpc.clone();
                std::thread::spawn(move || {
                    rpc.dispatch(
                        "Fs::atomic_write",
                        &json!({
                            "path": "/workspace/settings.json",
                            "text": format!("value-{index}")
                        }),
                    )
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().map_err(|_| anyhow!("writer panicked"))??;
        }
        let content = fs::read_to_string(root.path().join("settings.json"))?;
        assert!(content.starts_with("value-"));
        Ok(())
    }

    #[test]
    fn directory_tree_lists_but_does_not_prefetch_generated_directories() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("src/nested"))?;
        fs::create_dir_all(root.path().join("node_modules/package"))?;
        fs::write(root.path().join("src/nested/main.rs"), "fn main() {}")?;
        fs::write(
            root.path().join("node_modules/package/index.js"),
            "module.exports = {};",
        )?;
        let rpc = FsRpc::new(root.path().to_path_buf(), false)?;

        let tree = rpc.dispatch("Fs::read_dir_tree", &json!({"path": "/workspace"}))?;
        let entries = tree["entries"].as_array().context("missing root entries")?;
        assert!(entries.iter().any(|entry| entry == "/workspace/src"));
        assert!(
            entries
                .iter()
                .any(|entry| entry == "/workspace/node_modules")
        );
        assert!(tree["directories"].get("/workspace/src").is_some());
        assert!(tree["directories"].get("/workspace/src/nested").is_some());
        assert!(tree["directories"].get("/workspace/node_modules").is_none());
        Ok(())
    }
}
