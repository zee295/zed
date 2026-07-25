pub mod auth;
mod conn;
mod message_stream;
mod notification;
mod peer;
#[cfg(target_family = "wasm")]
pub mod wasm_conn;

pub use conn::Connection;
pub use notification::*;
pub use peer::*;
pub use proto;
pub use proto::{Receipt, TypedEnvelope, error::*};
mod macros;

#[cfg(feature = "gpui")]
mod proto_client;
#[cfg(feature = "gpui")]
pub use proto_client::*;

pub const PROTOCOL_VERSION: u32 = 68;
