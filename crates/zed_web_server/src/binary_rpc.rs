use anyhow::{Context as _, Result, bail, ensure};
use http_client::{github::AssetKind, github_download};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Component, Path};

use crate::fs_rpc::FsRpc;

#[derive(Deserialize)]
struct Download {
    url: String,
    digest: Option<String>,
    destination: String,
    kind: String,
    file_name: Option<String>,
}

pub async fn download(fs: &FsRpc, params: Value) -> Result<Value> {
    let request: Download = serde_json::from_value(params)?;
    let url = url::Url::parse(&request.url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "unsupported download URL scheme"
    );
    let destination = fs.path(&request.destination)?;
    ensure!(
        destination.file_name().is_some(),
        "invalid binary destination"
    );
    let client = reqwest_client::ReqwestClient::user_agent("zed-web")?;
    if request.kind == "raw" {
        let file_name = request.file_name.context("missing raw binary name")?;
        let mut components = Path::new(&file_name).components();
        ensure!(
            matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none()
                && !file_name.contains('\\'),
            "binary name must be a single filename"
        );
        github_download::download_server_raw_binary(
            &client,
            &request.url,
            request.digest.as_deref(),
            &destination,
            &file_name,
        )
        .await?;
    } else {
        let kind = match request.kind.as_str() {
            "tar.gz" => AssetKind::TarGz,
            "tar.bz2" => AssetKind::TarBz2,
            "gz" => AssetKind::Gz,
            "zip" => AssetKind::Zip,
            _ => bail!("unsupported binary archive kind"),
        };
        github_download::download_server_binary(
            &client,
            &request.url,
            request.digest.as_deref(),
            &destination,
            kind,
        )
        .await?;
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{io::Write, os::unix::fs::PermissionsExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/agent", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&body).await.unwrap();
        });
        (url, task)
    }

    #[tokio::test]
    async fn installs_raw_and_archive_agents_on_server_with_digest_verification() -> Result<()> {
        let root = tempfile::tempdir()?;
        let fs = FsRpc::new(root.path().to_path_buf(), true)?;
        let contents = b"#!/bin/sh\necho acp-ready\n";
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "agent", &contents[..])?;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar.into_inner()?)?;
        for (kind, body) in [("raw", contents.to_vec()), ("tar.gz", gz.finish()?)] {
            let digest = format!("{:X}", Sha256::digest(&body));
            let (url, task) = serve(body).await;
            download(&fs, json!({"url": url, "digest": digest, "destination": format!("/workspace/{kind}"), "kind": kind, "file_name": "agent"})).await?;
            task.await?;
            let installed = root.path().join(kind).join("agent");
            assert_eq!(std::fs::read(&installed)?, contents);
            assert_ne!(
                std::fs::metadata(&installed)?.permissions().mode() & 0o111,
                0
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_bad_digest_without_replacing_existing_installation() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("installed"))?;
        std::fs::write(root.path().join("installed/agent"), "old")?;
        let fs = FsRpc::new(root.path().to_path_buf(), true)?;
        let (url, task) = serve(b"new".to_vec()).await;
        let error = download(&fs, json!({"url": url, "digest": "00", "destination": "/workspace/installed", "kind": "raw", "file_name": "agent"})).await.unwrap_err();
        task.await?;
        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("installed/agent"))?,
            "old"
        );
        assert_eq!(std::fs::read_dir(root.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_unsafe_raw_names_and_restricted_destinations() -> Result<()> {
        let root = tempfile::tempdir()?;
        let fs = FsRpc::new(root.path().to_path_buf(), true)?;
        for name in ["../escape", "/escape", "a/b", "a\\b", ""] {
            assert!(download(&fs, json!({"url": "https://example.com/agent", "destination": "/workspace/install", "kind": "raw", "file_name": name})).await.is_err());
        }
        assert!(download(&fs, json!({"url": "https://example.com/agent", "destination": "/outside/install", "kind": "raw", "file_name": "agent"})).await.is_err());
        Ok(())
    }
}
