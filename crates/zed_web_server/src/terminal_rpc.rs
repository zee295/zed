use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, anyhow, bail};
use axum::extract::ws::Message;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::fs_rpc::FsRpc;

pub fn handles(method: &str) -> bool {
    method.starts_with("Terminal::")
}

pub struct TerminalManager {
    fs: Arc<FsRpc>,
    outgoing: mpsc::UnboundedSender<Message>,
    next_id: u64,
    terminals: HashMap<u64, TerminalEntry>,
}

struct TerminalEntry {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl TerminalManager {
    pub fn new(fs: Arc<FsRpc>, outgoing: mpsc::UnboundedSender<Message>) -> Self {
        Self {
            fs,
            outgoing,
            next_id: 1,
            terminals: HashMap::new(),
        }
    }

    pub fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "Terminal::open" => self.open(params),
            "Terminal::write" => self.write(params),
            "Terminal::resize" => self.resize(params),
            "Terminal::close" => self.close(params),
            _ => bail!("unknown terminal method: {method}"),
        }
    }

    fn open(&mut self, params: &Value) -> Result<Value> {
        let term_id = self.next_id;
        self.next_id += 1;
        let pair = native_pty_system().openpty(PtySize {
            rows: dimension(params, "rows", 24),
            cols: dimension(params, "cols", 80),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = params.get("shell").and_then(Value::as_object);
        let mut program = shell
            .and_then(|shell| shell.get("program"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let args = shell
            .and_then(|shell| shell.get("args"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if program.is_empty()
            || ((program == "/bin/sh" || program == "/bin/bash" || program == "sh")
                && args.is_empty())
        {
            program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        }
        let mut command = CommandBuilder::new(&program);
        if args.is_empty() {
            command.arg("-i");
        } else {
            command.args(args);
        }
        let root = self.fs.path("/workspace")?;
        let working_directory = params
            .get("working_directory")
            .and_then(Value::as_str)
            .map(|path| self.fs.path(path))
            .transpose()?
            .filter(|path| path.is_dir())
            .unwrap_or(root);
        command.cwd(&working_directory);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("ZED_REMOTE_TERMINAL", "1");
        for key in ["npm_config_prefix", "NPM_CONFIG_PREFIX"] {
            if command
                .get_env(key)
                .and_then(|value| value.to_str())
                .is_some_and(is_external_agent_npm_prefix)
            {
                command.env_remove(key);
            }
        }
        if let Some(environment) = params.get("env").and_then(Value::as_object) {
            for (key, value) in environment {
                if let Some(value) = value.as_str() {
                    if !key.eq_ignore_ascii_case("npm_config_prefix")
                        || !is_external_agent_npm_prefix(value)
                    {
                        command.env(key, value);
                    }
                }
            }
        }
        let mut child = pair.slave.spawn_command(command)?;
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let outgoing = self.outgoing.clone();
        std::thread::Builder::new()
            .name(format!("zed-terminal-reader-{term_id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 4096];
                loop {
                    let Ok(count) = reader.read(&mut buffer) else {
                        break;
                    };
                    if count == 0 {
                        break;
                    }
                    notify(
                        &outgoing,
                        &format!("Terminal::data:{term_id}"),
                        json!({
                            "term_id": term_id,
                            "data": BASE64.encode(&buffer[..count]),
                        }),
                    );
                }
            })?;
        let outgoing = self.outgoing.clone();
        std::thread::Builder::new()
            .name(format!("zed-terminal-wait-{term_id}"))
            .spawn(move || {
                let status = child.wait().ok().map(|status| status.exit_code() as i32);
                notify(
                    &outgoing,
                    &format!("Terminal::exit:{term_id}"),
                    json!({"term_id": term_id, "status": status}),
                );
            })?;
        self.terminals.insert(
            term_id,
            TerminalEntry {
                master: pair.master,
                writer: Mutex::new(writer),
                killer: Mutex::new(killer),
            },
        );
        Ok(json!({"term_id": term_id}))
    }

    fn write(&self, params: &Value) -> Result<Value> {
        let term_id = terminal_id(params)?;
        let Some(terminal) = self.terminals.get(&term_id) else {
            return Ok(Value::Null);
        };
        let bytes = BASE64.decode(
            params
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        let mut writer = terminal
            .writer
            .lock()
            .map_err(|_| anyhow!("terminal writer lock poisoned"))?;
        writer.write_all(&bytes)?;
        writer.flush()?;
        Ok(Value::Null)
    }

    fn resize(&self, params: &Value) -> Result<Value> {
        let term_id = terminal_id(params)?;
        if let Some(terminal) = self.terminals.get(&term_id) {
            terminal.master.resize(PtySize {
                rows: dimension(params, "rows", 24),
                cols: dimension(params, "cols", 80),
                pixel_width: 0,
                pixel_height: 0,
            })?;
        }
        Ok(Value::Null)
    }

    fn close(&mut self, params: &Value) -> Result<Value> {
        let term_id = terminal_id(params)?;
        if let Some(terminal) = self.terminals.remove(&term_id) {
            terminal
                .killer
                .lock()
                .map_err(|_| anyhow!("terminal child lock poisoned"))?
                .kill()
                .ok();
        }
        Ok(Value::Null)
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        for (_, terminal) in self.terminals.drain() {
            if let Ok(mut killer) = terminal.killer.lock() {
                killer.kill().ok();
            }
        }
    }
}

fn terminal_id(params: &Value) -> Result<u64> {
    params
        .get("term_id")
        .and_then(Value::as_u64)
        .context("missing terminal id")
}

fn dimension(params: &Value, key: &str, default: u16) -> u16 {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(default)
        .max(1)
}

fn is_external_agent_npm_prefix(value: &str) -> bool {
    value
        .replace('\\', "/")
        .contains("/external_agents/registry/npx/")
}

fn notify(outgoing: &mpsc::UnboundedSender<Message>, method: &str, params: Value) {
    outgoing
        .send(Message::Text(
            json!({"method": method, "params": params}).to_string(),
        ))
        .ok();
}

#[cfg(test)]
mod tests {
    use super::is_external_agent_npm_prefix;

    #[test]
    fn recognizes_external_agent_npm_prefix() {
        assert!(is_external_agent_npm_prefix(
            "/Users/zee/Library/Application Support/Zed/external_agents/registry/npx/codex-acp"
        ));
        assert!(is_external_agent_npm_prefix(
            r"C:\Users\zee\Zed\external_agents\registry\npx\codex-acp"
        ));
    }

    #[test]
    fn preserves_user_npm_prefix() {
        assert!(!is_external_agent_npm_prefix("/Users/zee/.npm-global"));
    }
}
