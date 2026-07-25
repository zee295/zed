#[cfg(not(target_family = "wasm"))]
mod codelldb;
mod gdb;
#[cfg(not(target_family = "wasm"))]
mod go;
#[cfg(not(target_family = "wasm"))]
mod javascript;
#[cfg(not(target_family = "wasm"))]
mod python;

#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
#[cfg(not(target_family = "wasm"))]
use codelldb::CodeLldbDebugAdapter;
use dap::{
    DapRegistry,
    adapters::{
        self, AdapterVersion, DapDelegate, DebugAdapter, DebugAdapterBinary, DebugAdapterName,
    },
    configure_tcp_connection,
};
use gdb::GdbDebugAdapter;
#[cfg(not(target_family = "wasm"))]
use go::GoDebugAdapter;
use gpui::{App, BorrowAppContext};
#[cfg(not(target_family = "wasm"))]
use javascript::JsDebugAdapter;
#[cfg(not(target_family = "wasm"))]
use python::PythonDebugAdapter;
use serde_json::json;
use task::{DebugScenario, ZedDebugConfig};

pub fn init(cx: &mut App) {
    cx.update_default_global(|registry: &mut DapRegistry, _cx| {
        #[cfg(not(target_family = "wasm"))]
        registry.add_adapter(Arc::from(CodeLldbDebugAdapter::default()));
        #[cfg(not(target_family = "wasm"))]
        registry.add_adapter(Arc::from(PythonDebugAdapter::default()));
        #[cfg(not(target_family = "wasm"))]
        registry.add_adapter(Arc::from(JsDebugAdapter::default()));
        #[cfg(not(target_family = "wasm"))]
        registry.add_adapter(Arc::from(GoDebugAdapter::default()));
        registry.add_adapter(Arc::from(GdbDebugAdapter));

        #[cfg(any(test, feature = "test-support"))]
        {
            registry.add_adapter(Arc::from(dap::FakeAdapter {}));
        }
    })
}

#[cfg(test)]
mod test_mocks {
    use super::*;

    pub(crate) struct MockDelegate {
        worktree_root: PathBuf,
    }

    impl MockDelegate {
        pub(crate) fn new() -> Arc<dyn adapters::DapDelegate> {
            Arc::new(Self {
                worktree_root: PathBuf::from("/tmp/test"),
            })
        }
    }

    #[async_trait::async_trait]
    impl adapters::DapDelegate for MockDelegate {
        fn worktree_id(&self) -> settings::WorktreeId {
            settings::WorktreeId::from_usize(0)
        }

        fn worktree_root_path(&self) -> &std::path::Path {
            &self.worktree_root
        }

        fn http_client(&self) -> Arc<dyn http_client::HttpClient> {
            unimplemented!("Not needed for tests")
        }

        fn node_runtime(&self) -> node_runtime::NodeRuntime {
            unimplemented!("Not needed for tests")
        }

        fn toolchain_store(&self) -> Arc<dyn language::LanguageToolchainStore> {
            unimplemented!("Not needed for tests")
        }

        fn fs(&self) -> Arc<dyn fs::Fs> {
            unimplemented!("Not needed for tests")
        }

        fn output_to_console(&self, _msg: String) {}

        async fn which(&self, _command: &std::ffi::OsStr) -> Option<PathBuf> {
            None
        }

        async fn read_text_file(&self, _path: &util::rel_path::RelPath) -> Result<String> {
            Ok(String::new())
        }

        async fn shell_env(&self) -> collections::HashMap<String, String> {
            collections::HashMap::default()
        }

        fn is_headless(&self) -> bool {
            false
        }
    }
}
