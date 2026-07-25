use crate::ResultExt;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

#[cfg(not(target_family = "wasm"))]
use async_fs as fs;
#[cfg(not(target_family = "wasm"))]
use futures_lite::StreamExt;

/// Removes all files and directories matching the given predicate
#[cfg(not(target_family = "wasm"))]
pub async fn remove_matching<F>(dir: &Path, predicate: F)
where
    F: Fn(&Path) -> bool,
{
    if let Some(mut entries) = fs::read_dir(dir).await.log_err() {
        while let Some(entry) = entries.next().await {
            if let Some(entry) = entry.log_err() {
                let entry_path = entry.path();
                if predicate(entry_path.as_path())
                    && let Ok(metadata) = fs::metadata(&entry_path).await
                {
                    if metadata.is_file() {
                        fs::remove_file(&entry_path).await.log_err();
                    } else {
                        fs::remove_dir_all(&entry_path).await.log_err();
                    }
                }
            }
        }
    }
}

#[cfg(target_family = "wasm")]
pub async fn remove_matching<F>(_dir: &Path, _predicate: F)
where
    F: Fn(&Path) -> bool,
{
}

#[cfg(not(target_family = "wasm"))]
pub async fn collect_matching<F>(dir: &Path, predicate: F) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let mut matching = vec![];

    if let Some(mut entries) = fs::read_dir(dir).await.log_err() {
        while let Some(entry) = entries.next().await {
            if let Some(entry) = entry.log_err()
                && predicate(entry.path().as_path())
            {
                matching.push(entry.path());
            }
        }
    }

    matching
}

#[cfg(target_family = "wasm")]
pub async fn collect_matching<F>(_dir: &Path, _predicate: F) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    Vec::new()
}

#[cfg(not(target_family = "wasm"))]
pub async fn find_file_name_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&str) -> bool,
{
    if let Some(mut entries) = fs::read_dir(dir).await.log_err() {
        while let Some(entry) = entries.next().await {
            if let Some(entry) = entry.log_err() {
                let entry_path = entry.path();

                if let Some(file_name) = entry_path
                    .file_name()
                    .map(|file_name| file_name.to_string_lossy())
                    && predicate(&file_name)
                {
                    return Some(entry_path);
                }
            }
        }
    }

    None
}

#[cfg(target_family = "wasm")]
pub async fn find_file_name_in_dir<F>(_dir: &Path, _predicate: F) -> Option<PathBuf>
where
    F: Fn(&str) -> bool,
{
    None
}

#[cfg(not(target_family = "wasm"))]
pub async fn move_folder_files_to_folder<P: AsRef<Path>>(
    source_path: P,
    target_path: P,
) -> Result<()> {
    if !target_path.as_ref().is_dir() {
        bail!("Folder not found or is not a directory");
    }

    let mut entries = fs::read_dir(source_path.as_ref()).await?;
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let old_path = entry.path();
        let new_path = target_path.as_ref().join(entry.file_name());

        fs::rename(&old_path, &new_path).await?;
    }

    fs::remove_dir(source_path).await?;

    Ok(())
}

#[cfg(target_family = "wasm")]
pub async fn move_folder_files_to_folder<P: AsRef<Path>>(
    _source_path: P,
    _target_path: P,
) -> Result<()> {
    bail!("filesystem operations are not supported in the browser")
}

#[cfg(unix)]
/// Set the permissions for the given path so that the file becomes executable.
/// This is a noop for non-unix platforms.
pub async fn make_file_executable(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(
        path,
        <fs::Permissions as fs::unix::PermissionsExt>::from_mode(0o755),
    )
    .await
}

#[cfg(not(unix))]
#[allow(clippy::unused_async)]
/// Set the permissions for the given path so that the file becomes executable.
/// This is a noop for non-unix platforms.
pub async fn make_file_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
