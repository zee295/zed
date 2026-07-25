use std::path::Path;

use anyhow::{Result, anyhow};

use crate::{HttpClient, github::AssetKind};

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GithubBinaryMetadata {
    pub metadata_version: u64,
    pub digest: Option<String>,
}

impl GithubBinaryMetadata {
    pub async fn read_from_file(_metadata_path: &Path) -> Result<GithubBinaryMetadata> {
        Err(anyhow!(
            "GitHub binary download is not available in the browser"
        ))
    }

    pub async fn write_to_file(&self, _metadata_path: &Path) -> Result<()> {
        Err(anyhow!(
            "GitHub binary download is not available in the browser"
        ))
    }
}

pub async fn download_server_binary(
    _http_client: &dyn HttpClient,
    _url: &str,
    _digest: Option<&str>,
    _destination_path: &Path,
    _asset_kind: AssetKind,
) -> Result<(), anyhow::Error> {
    Err(anyhow!(
        "GitHub binary download is not available in the browser"
    ))
}

pub async fn download_server_raw_binary(
    _http_client: &dyn HttpClient,
    _url: &str,
    _digest: Option<&str>,
    _destination_path: &Path,
    _binary_file_name: &str,
) -> Result<(), anyhow::Error> {
    Err(anyhow!(
        "GitHub binary download is not available in the browser"
    ))
}
