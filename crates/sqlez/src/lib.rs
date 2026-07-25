pub mod bindable;

#[cfg(not(target_family = "wasm"))]
pub mod connection;
#[cfg(target_family = "wasm")]
pub mod connection_wasm;
#[cfg(target_family = "wasm")]
pub use connection_wasm as connection;

pub mod domain;

#[cfg(not(target_family = "wasm"))]
pub mod migrations;
#[cfg(target_family = "wasm")]
pub mod migrations_wasm;
#[cfg(target_family = "wasm")]
pub use migrations_wasm as migrations;

/// Sync HTTP bridge to server-side SQLite (WASM only).
#[cfg(target_family = "wasm")]
pub mod remote_sql;

pub mod savepoint;

#[cfg(not(target_family = "wasm"))]
pub mod statement;
#[cfg(target_family = "wasm")]
pub mod statement_wasm;
#[cfg(target_family = "wasm")]
pub use statement_wasm as statement;

pub mod thread_safe_connection;
pub mod typed_statements;
mod util;

pub use anyhow;
