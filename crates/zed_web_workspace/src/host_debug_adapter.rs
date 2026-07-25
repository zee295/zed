use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use dap::{
    DapRegistry, DebugRequest, StartDebuggingRequestArguments,
    adapters::{
        DapDelegate, DebugAdapter, DebugAdapterBinary, DebugAdapterName, DebugTaskDefinition,
    },
};
use gpui::{App, AsyncApp, BorrowAppContext};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use task::{DebugScenario, ZedDebugConfig};

const HOST_ADAPTER_COMMAND: &str = "zed-web-debug-adapter";

struct HostDebugAdapter {
    name: &'static str,
    key: &'static str,
}

pub fn init(cx: &mut App) {
    cx.update_default_global(|registry: &mut DapRegistry, _cx| {
        for (name, key) in [
            ("CodeLLDB", "lldb"),
            ("Debugpy", "debugpy"),
            ("JavaScript", "javascript"),
            ("Delve", "delve"),
        ] {
            registry.add_adapter(Arc::new(HostDebugAdapter { name, key }));
        }
    });
}

#[async_trait(?Send)]
impl DebugAdapter for HostDebugAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(self.name.into())
    }

    async fn config_from_zed_format(&self, scenario: ZedDebugConfig) -> Result<DebugScenario> {
        let mut config = json!({
            "request": match &scenario.request {
                DebugRequest::Launch(_) => "launch",
                DebugRequest::Attach(_) => "attach",
            }
        });
        let values = config
            .as_object_mut()
            .expect("debug configuration is an object");

        match &scenario.request {
            DebugRequest::Launch(launch) => {
                values.insert("program".into(), launch.program.clone().into());
                values.insert("args".into(), launch.args.clone().into());
                if !launch.env.is_empty() {
                    values.insert("env".into(), launch.env_json());
                }
                if let Some(cwd) = &launch.cwd {
                    values.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
                }
                if let Some(stop_on_entry) = scenario.stop_on_entry {
                    values.insert("stopOnEntry".into(), stop_on_entry.into());
                }
            }
            DebugRequest::Attach(attach) => {
                values.insert("pid".into(), attach.process_id.into());
            }
        }

        Ok(DebugScenario {
            adapter: scenario.adapter,
            label: scenario.label,
            build: None,
            tcp_connection: None,
            config,
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        definition: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        _cx: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        let mut configuration = definition.config.clone();
        if let Some(values) = configuration.as_object_mut() {
            values
                .entry("cwd")
                .or_insert_with(|| delegate.worktree_root_path().to_string_lossy().into());
            if self.key == "javascript" {
                values.entry("type").and_modify(|value| {
                    if let Some(kind) = value.as_str() {
                        let normalized = match kind {
                            "node" | "pwa-node" | "node-terminal" => "pwa-node",
                            "chrome" | "pwa-chrome" => "pwa-chrome",
                            "edge" | "msedge" | "pwa-edge" | "pwa-msedge" => "pwa-msedge",
                            other => other,
                        };
                        *value = normalized.into();
                    }
                });
            }
        }

        let mut envs = delegate.shell_env().await;
        envs.extend(user_env.unwrap_or_default());

        let (command, arguments) = if let Some(path) = user_installed_path {
            (
                path.to_string_lossy().into_owned(),
                user_args.unwrap_or_default(),
            )
        } else {
            let mut arguments = vec![self.key.to_string()];
            arguments.extend(user_args.unwrap_or_default());
            (HOST_ADAPTER_COMMAND.to_string(), arguments)
        };

        Ok(DebugAdapterBinary {
            command: Some(command),
            arguments,
            envs,
            cwd: Some(delegate.worktree_root_path().to_path_buf()),
            connection: None,
            request_args: StartDebuggingRequestArguments {
                request: self.request_kind(&configuration).await?,
                configuration,
            },
        })
    }

    fn dap_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["request"],
            "properties": {
                "request": {
                    "type": "string",
                    "enum": ["launch", "attach"]
                },
                "program": {
                    "type": "string",
                    "description": "Program or script to debug"
                },
                "module": {
                    "type": "string",
                    "description": "Module to run"
                },
                "pid": {
                    "type": ["integer", "string"],
                    "description": "Process id to attach to"
                },
                "processId": {
                    "type": ["integer", "string"],
                    "description": "Process id to attach to"
                },
                "type": {
                    "type": "string"
                },
                "runtimeExecutable": {
                    "type": "string"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "default": []
                },
                "cwd": {
                    "type": "string",
                    "default": "${ZED_WORKTREE_ROOT}"
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                },
                "stopOnEntry": {
                    "type": "boolean",
                    "default": false
                }
            }
        })
    }
}
