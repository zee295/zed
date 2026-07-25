#[cfg(target_family = "wasm")]
pub mod fs;
#[cfg(target_family = "wasm")]
pub mod git;
#[cfg(target_family = "wasm")]
pub mod transport;

#[cfg(target_family = "wasm")]
use std::sync::OnceLock;

#[cfg(target_family = "wasm")]
pub use fs::RemoteFs;
#[cfg(target_family = "wasm")]
pub use git::RemoteGitRepository;
#[cfg(target_family = "wasm")]
pub use transport::RemoteClient;

#[cfg(target_family = "wasm")]
static REMOTE_CLIENT: OnceLock<RemoteClient> = OnceLock::new();

#[cfg(target_family = "wasm")]
pub fn set_remote_client(client: RemoteClient) {
    let _ = REMOTE_CLIENT.set(client);
}

#[cfg(target_family = "wasm")]
pub fn remote_client() -> Option<RemoteClient> {
    REMOTE_CLIENT.get().cloned()
}
