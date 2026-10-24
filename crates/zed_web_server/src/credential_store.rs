use std::{io::Write as _, path::Path};

use anyhow::{Context as _, Result, bail};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::{AppState, security_headers};

#[derive(Deserialize)]
pub struct CredentialRequest {
    operation: String,
    url: String,
    username: Option<String>,
    password: Option<Vec<u8>>,
}

pub async fn request(
    State(state): State<AppState>,
    Json(request): Json<CredentialRequest>,
) -> Response {
    let result =
        tokio::task::spawn_blocking(move || dispatch(&state.root, &state.auth_token, request))
            .await
            .context("credential storage worker failed")
            .and_then(|result| result);
    let mut response = match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            tracing::warn!(%error, "provider credential storage failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Credential storage failed",
            )
                .into_response()
        }
    };
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    security_headers(response)
}

fn dispatch(root: &Path, token: &str, request: CredentialRequest) -> Result<serde_json::Value> {
    if request.url.is_empty() || request.url.len() > 4096 {
        bail!("invalid credential key");
    }
    let directory = root.join(".zed/web-credentials");
    let name = hex::encode(Sha256::digest(request.url.as_bytes()));
    let path = directory.join(name);
    let key_material = Sha256::digest(format!("zed-web-credentials-v1:{token}"));
    let key = LessSafeKey::new(
        UnboundKey::new(&aead::AES_256_GCM, &key_material)
            .map_err(|_| anyhow::anyhow!("invalid encryption key"))?,
    );
    match request.operation.as_str() {
        "read" => {
            let mut bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(serde_json::Value::Null);
                }
                Err(error) => return Err(error.into()),
            };
            if bytes.len() < aead::NONCE_LEN + aead::AES_256_GCM.tag_len() {
                bail!("invalid encrypted credential");
            }
            let nonce: [u8; aead::NONCE_LEN] = bytes
                .drain(..aead::NONCE_LEN)
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid nonce"))?;
            let plain = key
                .open_in_place(
                    Nonce::assume_unique_for_key(nonce),
                    Aad::from(request.url.as_bytes()),
                    &mut bytes,
                )
                .map_err(|_| anyhow::anyhow!("credential decryption failed"))?;
            Ok(serde_json::from_slice(plain)?)
        }
        "write" => {
            let username = request.username.context("missing username")?;
            let password = request.password.context("missing password")?;
            if username.len() + password.len() > 64 * 1024 {
                bail!("credential is too large");
            }
            std::fs::create_dir_all(&directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            }
            std::fs::write(directory.join(".gitignore"), "*\n")?;
            let mut bytes = serde_json::to_vec(&(username, password))?;
            let mut nonce = [0; aead::NONCE_LEN];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| anyhow::anyhow!("nonce generation failed"))?;
            key.seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(request.url.as_bytes()),
                &mut bytes,
            )
            .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            file.write_all(&nonce)?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            file.persist(path)?;
            Ok(serde_json::Value::Null)
        }
        "delete" => {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(serde_json::Value::Null)
        }
        _ => bail!("unknown credential operation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(operation: &str, url: &str) -> CredentialRequest {
        CredentialRequest {
            operation: operation.into(),
            url: url.into(),
            username: None,
            password: None,
        }
    }

    #[test]
    fn persists_encrypted_credentials_and_deletes_them() -> Result<()> {
        let root = tempfile::tempdir()?;
        let initialized = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .output()?;
        anyhow::ensure!(
            initialized.status.success(),
            "failed to initialize test repository"
        );
        let url = "https://chatgpt.com/backend-api/codex";
        let mut write = operation("write", url);
        write.username = Some("Bearer".into());
        write.password = Some(b"secret-refresh-token".to_vec());
        dispatch(root.path(), "server-token", write)?;
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()?;
        assert!(status.status.success());
        assert!(
            status.stdout.is_empty(),
            "credentials must never appear in Git status"
        );
        let stored = std::fs::read(
            root.path()
                .join(".zed/web-credentials")
                .join(hex::encode(Sha256::digest(url))),
        )?;
        assert!(
            !stored
                .windows(b"secret-refresh-token".len())
                .any(|window| window == b"secret-refresh-token")
        );
        assert_eq!(
            dispatch(root.path(), "server-token", operation("read", url))?,
            serde_json::json!(["Bearer", b"secret-refresh-token".to_vec()])
        );
        assert!(dispatch(root.path(), "wrong-token", operation("read", url)).is_err());
        dispatch(root.path(), "server-token", operation("delete", url))?;
        assert!(dispatch(root.path(), "server-token", operation("read", url))?.is_null());
        Ok(())
    }

    #[test]
    fn credential_keys_cannot_escape_storage_or_read_other_slots() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut write = operation("write", "../../outside");
        write.username = Some("Bearer".into());
        write.password = Some(vec![1, 2, 3]);
        dispatch(root.path(), "token", write)?;
        assert_eq!(
            std::fs::read_dir(root.path().join(".zed/web-credentials"))?.count(),
            2
        );
        assert!(dispatch(root.path(), "token", operation("read", "other"))?.is_null());
        Ok(())
    }
}
