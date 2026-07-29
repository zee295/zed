//! WASM filesystem shim for `smol::fs`.
//!
//! Routes operations through the same remote JSON-RPC client used by
//! `smol::process` (`smol::set_remote_client`). Paths are virtual server paths
//! such as `/workspace/...`.
//!
//! Note: `std::fs::Metadata` cannot be constructed on `wasm32-unknown-unknown`
//! (its platform type is uninhabited). Call sites that need type info should
//! use `Fs::is_dir` / `Fs::metadata` on the high-level `RemoteFs` trait, or the
//! helpers below that return booleans / lengths.

use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

use crate::process::remote_rpc_client;

fn io_err(err: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn rpc_call<R: serde::de::DeserializeOwned>(
    method: &str,
    params: &serde_json::Value,
) -> io::Result<R> {
    let client = remote_rpc_client()?;
    client.call(method, params).await.map_err(io_err)
}

async fn rpc_void(method: &str, params: &serde_json::Value) -> io::Result<()> {
    let client = remote_rpc_client()?;
    client.call_void(method, params).await.map_err(io_err)
}

fn unsupported_metadata<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "std::fs::Metadata cannot be synthesized on WASM; use RemoteFs / Fs::is_dir instead",
    ))
}

pub struct File {
    path: PathBuf,
    data: Vec<u8>,
    pos: usize,
}

impl File {
    pub async fn create(path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref().to_path_buf();
        rpc_void(
            "Fs::create_file",
            &json!({
                "path": path_str(&path),
                "overwrite": true,
                "ignore_if_exists": false,
            }),
        )
        .await?;
        Ok(File {
            path,
            data: Vec::new(),
            pos: 0,
        })
    }

    pub async fn open(path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref().to_path_buf();
        ensure_file(&path).await?;
        let encoded: String =
            rpc_call("Fs::load_bytes", &json!({ "path": path_str(&path) })).await?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(File { path, data, pos: 0 })
    }

    pub async fn read_to_string(&mut self, buf: &mut String) -> io::Result<usize> {
        let start = self.pos;
        let s = String::from_utf8_lossy(&self.data[start..]);
        let n = s.len();
        buf.push_str(&s);
        self.pos = self.data.len();
        Ok(n)
    }
}

impl futures::io::AsyncRead for File {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let remaining = self.data.len().saturating_sub(self.pos);
        if remaining == 0 {
            return std::task::Poll::Ready(Ok(0));
        }
        let n = remaining.min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        std::task::Poll::Ready(Ok(n))
    }
}

impl futures::io::AsyncWrite for File {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.data.extend_from_slice(buf);
        self.pos = self.data.len();
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        // Flush is async-remote; callers that need durable writes should use
        // `smol::fs::write` / RemoteFs::atomic_write.
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    truncate: bool,
    append: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        OpenOptions {
            read: false,
            write: false,
            create: false,
            truncate: false,
            append: false,
        }
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        if self.create || self.write || self.truncate {
            if self.truncate || self.create {
                let _ = File::create(path).await?;
            }
        }
        if self.read || !(self.write || self.create) {
            File::open(path).await
        } else {
            Ok(File {
                path: path.to_path_buf(),
                data: Vec::new(),
                pos: 0,
            })
        }
    }
}

pub async fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    ensure_file(path.as_ref()).await?;
    let encoded: String = rpc_call(
        "Fs::load_bytes",
        &json!({ "path": path_str(path.as_ref()) }),
    )
    .await?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    ensure_file(path.as_ref()).await?;
    rpc_call("Fs::load", &json!({ "path": path_str(path.as_ref()) })).await
}

async fn ensure_file(path: &Path) -> io::Result<()> {
    if is_file(path).await? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("file not found: {}", path.display()),
        ))
    }
}

pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(contents.as_ref());
    rpc_void(
        "Fs::write",
        &json!({
            "path": path_str(path.as_ref()),
            "content": encoded,
        }),
    )
    .await
}

pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let data = read(from).await?;
    let len = data.len() as u64;
    write(to, data).await?;
    Ok(len)
}

pub async fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    rpc_void(
        "Fs::create_dir",
        &json!({ "path": path_str(path.as_ref()) }),
    )
    .await
}

pub async fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    // Server create_dir already creates parents when needed for files; for
    // directories walk components and create each segment.
    let path = path.as_ref();
    let mut cur = PathBuf::new();
    for comp in path.components() {
        cur.push(comp);
        let s = path_str(&cur);
        if s.is_empty() || s == "/" {
            continue;
        }
        let is_dir: bool = rpc_call("Fs::is_dir", &json!({ "path": s }))
            .await
            .unwrap_or(false);
        if !is_dir {
            let _ = create_dir(&cur).await;
        }
    }
    Ok(())
}

pub async fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    rpc_void(
        "Fs::remove_dir",
        &json!({
            "path": path_str(path.as_ref()),
            "recursive": false,
            "ignore_if_not_exists": false,
        }),
    )
    .await
}

pub async fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    rpc_void(
        "Fs::remove_dir",
        &json!({
            "path": path_str(path.as_ref()),
            "recursive": true,
            "ignore_if_not_exists": false,
        }),
    )
    .await
}

pub async fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    rpc_void(
        "Fs::remove_file",
        &json!({
            "path": path_str(path.as_ref()),
            "recursive": false,
            "ignore_if_not_exists": false,
        }),
    )
    .await
}

pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    rpc_void(
        "Fs::rename",
        &json!({
            "source": path_str(from.as_ref()),
            "target": path_str(to.as_ref()),
            "overwrite": true,
            "ignore_if_exists": false,
            "create_parents": true,
        }),
    )
    .await
}

/// Not available: `std::fs::Metadata` is uninhabited on wasm32-unknown-unknown.
pub async fn metadata(_path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    unsupported_metadata()
}

pub async fn is_file(path: impl AsRef<Path>) -> io::Result<bool> {
    rpc_call("Fs::is_file", &json!({ "path": path_str(path.as_ref()) })).await
}

pub async fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    metadata(path).await
}

#[derive(Deserialize)]
struct ReadDirResponse {
    entries: Vec<String>,
}

pub async fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let response: ReadDirResponse =
        rpc_call("Fs::read_dir", &json!({ "path": path_str(path.as_ref()) })).await?;
    Ok(ReadDir {
        entries: response.entries.into_iter().map(PathBuf::from).collect(),
        index: 0,
    })
}

pub struct ReadDir {
    entries: Vec<PathBuf>,
    index: usize,
}

impl futures_lite::stream::Stream for ReadDir {
    type Item = io::Result<DirEntry>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.index >= self.entries.len() {
            return std::task::Poll::Ready(None);
        }
        let path = self.entries[self.index].clone();
        self.index += 1;
        std::task::Poll::Ready(Some(Ok(DirEntry { path })))
    }
}

pub struct DirEntry {
    path: PathBuf,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> std::ffi::OsString {
        self.path.file_name().unwrap_or_default().to_os_string()
    }
}

pub async fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let result: String = rpc_call(
        "Fs::canonicalize",
        &json!({ "path": path_str(path.as_ref()) }),
    )
    .await?;
    Ok(PathBuf::from(result))
}

pub async fn hard_link(_original: impl AsRef<Path>, _link: impl AsRef<Path>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hard_link not supported over remote fs",
    ))
}

pub async fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let result: String =
        rpc_call("Fs::read_link", &json!({ "path": path_str(path.as_ref()) })).await?;
    Ok(PathBuf::from(result))
}

pub async fn set_permissions(
    _path: impl AsRef<Path>,
    _perm: std::fs::Permissions,
) -> io::Result<()> {
    // No-op: remote server owns permissions.
    Ok(())
}

pub async fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    rpc_void(
        "Fs::create_symlink",
        &json!({
            "path": path_str(link.as_ref()),
            "target": path_str(original.as_ref()),
        }),
    )
    .await
}

pub async fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    symlink_file(original, link).await
}

pub mod unix {
    use std::io;
    use std::path::Path;

    pub async fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
        super::symlink_file(src, dst).await
    }
}

pub mod windows {
    use std::io;
    use std::path::Path;

    pub async fn symlink_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
        super::symlink_file(src, dst).await
    }

    pub async fn symlink_dir(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
        super::symlink_dir(src, dst).await
    }
}
