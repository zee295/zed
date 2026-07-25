use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::HttpClient;

pub struct GitHubLspBinaryVersion {
    pub name: String,
    pub url: String,
    pub digest: Option<String>,
}

#[derive(Debug)]
pub struct GithubRelease {
    pub tag_name: String,
    pub pre_release: bool,
    pub assets: Vec<GithubReleaseAsset>,
    pub tarball_url: String,
    pub zipball_url: String,
}

#[derive(Debug)]
pub struct GithubReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub digest: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AssetKind {
    TarGz,
    TarBz2,
    Gz,
    Zip,
}

pub async fn latest_github_release(
    _repo_name_with_owner: &str,
    _require_assets: bool,
    _pre_release: bool,
    _http: Arc<dyn HttpClient>,
) -> Result<GithubRelease> {
    Err(anyhow!("GitHub API is not available in the browser"))
}

pub async fn get_release_by_tag_name(
    _repo_name_with_owner: &str,
    _tag: &str,
    _http: Arc<dyn HttpClient>,
) -> Result<GithubRelease> {
    Err(anyhow!("GitHub API is not available in the browser"))
}

pub fn build_asset_url(
    _repo_name_with_owner: &str,
    _tag: &str,
    _kind: AssetKind,
) -> Result<String> {
    Err(anyhow!("GitHub API is not available in the browser"))
}
