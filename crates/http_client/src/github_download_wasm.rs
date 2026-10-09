use std::{path::Path, sync::OnceLock};

use anyhow::{Result, anyhow};
use serde_json::json;
use wasm_rpc::RpcClient;

use crate::{HttpClient, github::AssetKind};

static CLIENT: OnceLock<RpcClient> = OnceLock::new();

pub fn set_remote_client(client: RpcClient) {
    let _ = CLIENT.set(client);
}

fn client() -> Result<&'static RpcClient> {
    CLIENT
        .get()
        .ok_or_else(|| anyhow!("binary download server is not connected"))
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GithubBinaryMetadata {
    pub metadata_version: u64,
    pub digest: Option<String>,
}

impl GithubBinaryMetadata {
    pub async fn read_from_file(metadata_path: &Path) -> Result<Self> {
        let content: String = client()?
            .call("Fs::load", &json!({"path": metadata_path}))
            .await?;
        Ok(serde_json::from_str(&content)?)
    }

    pub async fn write_to_file(&self, metadata_path: &Path) -> Result<()> {
        client()?
            .call_void(
                "Fs::atomic_write",
                &json!({
                    "path": metadata_path, "text": serde_json::to_string(self)?,
                }),
            )
            .await
    }
}

pub async fn download_server_binary(
    _http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    let kind = match asset_kind {
        AssetKind::TarGz => "tar.gz",
        AssetKind::TarBz2 => "tar.bz2",
        AssetKind::Gz => "gz",
        AssetKind::Zip => "zip",
    };
    client()?
        .call_void(
            "Binary::download",
            &json!({
                "url": url, "digest": digest, "destination": destination_path, "kind": kind,
            }),
        )
        .await
}

pub async fn download_server_raw_binary(
    _http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    binary_file_name: &str,
) -> Result<()> {
    client()?
        .call_void(
            "Binary::download",
            &json!({
                "url": url, "digest": digest, "destination": destination_path,
                "kind": "raw", "file_name": binary_file_name,
            }),
        )
        .await
}
