//! WASM `Connection` backed by server-side SQLite over HTTP `/sql`.

use std::cell::RefCell;
use std::path::Path;

use anyhow::Result;

pub struct Connection {
    /// Logical DB name (for shared in-memory uri compatibility).
    #[allow(dead_code)]
    uri: String,
    persistent: bool,
    pub(crate) write: RefCell<bool>,
}

unsafe impl Send for Connection {}

impl Connection {
    pub(crate) fn open(uri: &str, persistent: bool) -> Result<Self> {
        Ok(Self {
            uri: uri.to_string(),
            persistent,
            write: RefCell::new(true),
        })
    }

    pub fn open_file(uri: &str) -> Self {
        Self::open(uri, true).expect("open_file")
    }

    pub fn open_memory(uri: Option<&str>) -> Self {
        Self {
            uri: uri.unwrap_or(":memory:").to_string(),
            persistent: false,
            write: RefCell::new(true),
        }
    }

    pub fn persistent(&self) -> bool {
        self.persistent
    }

    pub fn can_write(&self) -> bool {
        *self.write.borrow()
    }

    pub fn backup_main(&self, _destination: &Connection) -> Result<()> {
        Ok(())
    }

    pub fn backup_main_to(&self, _destination: impl AsRef<Path>) -> Result<()> {
        Ok(())
    }

    pub fn sql_has_syntax_error(&self, _sql: &str) -> Option<(String, usize)> {
        None
    }

    pub(crate) fn last_error(&self) -> Result<()> {
        Ok(())
    }

    pub(crate) fn with_write<T>(&self, callback: impl FnOnce(&Connection) -> T) -> T {
        *self.write.borrow_mut() = true;
        let result = callback(self);
        *self.write.borrow_mut() = false;
        result
    }
}
