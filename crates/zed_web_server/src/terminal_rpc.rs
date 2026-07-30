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
    terminals_by_resume_key: HashMap<String, u64>,
    #[cfg(unix)]
    open_url_bridge: Option<crate::process_rpc::OpenUrlBridge>,
}

struct TerminalEntry {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Arc<Mutex<TerminalOutput>>,
    resume_key: Option<String>,
}

struct TerminalOutput {
    history: Vec<u8>,
    data_method: String,
    exit_method: String,
    exit_status: Option<i32>,
}

impl TerminalManager {
    pub fn new(fs: Arc<FsRpc>, outgoing: mpsc::UnboundedSender<Message>) -> Self {
        #[cfg(unix)]
        let open_url_bridge = tokio::runtime::Handle::try_current().ok().and_then(|_| {
            crate::process_rpc::OpenUrlBridge::new(outgoing.clone())
                .map_err(|error| tracing::warn!(?error, "failed to initialize terminal URL bridge"))
                .ok()
        });
        Self {
            fs,
            outgoing,
            next_id: 1,
            terminals: HashMap::new(),
            terminals_by_resume_key: HashMap::new(),
            #[cfg(unix)]
            open_url_bridge,
        }
    }

    pub fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "Terminal::open" => self.open(params),
            "Terminal::write" => self.write(params),
            "Terminal::resize" => self.resize(params),
            "Terminal::bind" => self.bind(params),
            "Terminal::attach" => self.attach(params),
            "Terminal::close" => self.close(params),
            _ => bail!("unknown terminal method: {method}"),
        }
    }

    fn open(&mut self, params: &Value) -> Result<Value> {
        let resume_key = params
            .get("resume_key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .map(ToOwned::to_owned);
        if let Some(resume_key) = resume_key.as_deref()
            && let Some(term_id) = self.resume_terminal(resume_key, params)?
        {
            return Ok(term_id);
        }

        let root = self.fs.path("/workspace")?;
        let rewrite = |value: &str| self.fs.rewrite_legacy_workspace_path(value);
        let term_id = self.next_id;
        self.next_id += 1;
        let (data_method, exit_method) = notification_methods(params, term_id);
        let output = Arc::new(Mutex::new(TerminalOutput {
            history: Vec::new(),
            data_method,
            exit_method,
            exit_status: None,
        }));
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
            .map(rewrite)
            .unwrap_or_default();
        let args = shell
            .and_then(|shell| shell.get("args"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(rewrite)
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
                        command.env(key, rewrite(value));
                    }
                }
            }
        }
        #[cfg(unix)]
        if let Some(bridge) = &self.open_url_bridge {
            bridge.configure_pty_command(&mut command)?;
        }
        let mut child = pair.slave.spawn_command(command)?;
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let outgoing = self.outgoing.clone();
        let reader_output = output.clone();
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
                    let method = {
                        let Ok(mut output) = reader_output.lock() else {
                            break;
                        };
                        output.history.extend_from_slice(&buffer[..count]);
                        output.data_method.clone()
                    };
                    notify(&outgoing, &method, terminal_data(term_id, &buffer[..count]));
                }
            })?;
        let outgoing = self.outgoing.clone();
        let wait_output = output.clone();
        std::thread::Builder::new()
            .name(format!("zed-terminal-wait-{term_id}"))
            .spawn(move || {
                let status = child.wait().ok().map(|status| status.exit_code() as i32);
                let method = wait_output
                    .lock()
                    .map(|mut output| {
                        output.exit_status = status;
                        output.exit_method.clone()
                    })
                    .unwrap_or_else(|_| format!("Terminal::exit:{term_id}"));
                notify(
                    &outgoing,
                    &method,
                    json!({"term_id": term_id, "status": status}),
                );
            })?;
        if let Some(resume_key) = resume_key.as_ref() {
            self.terminals_by_resume_key
                .insert(resume_key.clone(), term_id);
        }
        self.terminals.insert(
            term_id,
            TerminalEntry {
                master: pair.master,
                writer: Mutex::new(writer),
                killer: Mutex::new(killer),
                output,
                resume_key,
            },
        );
        Ok(json!({"term_id": term_id, "resumed": false}))
    }

    fn resume_terminal(&mut self, resume_key: &str, params: &Value) -> Result<Option<Value>> {
        let term_id = self.terminals_by_resume_key.get(resume_key).copied();
        let Some(term_id) = term_id else {
            return Ok(None);
        };
        let has_exited = self
            .terminals
            .get(&term_id)
            .map(|terminal| {
                terminal
                    .output
                    .lock()
                    .map(|output| output.exit_status.is_some())
                    .map_err(|_| anyhow!("terminal output lock poisoned"))
            })
            .transpose()?
            .unwrap_or(false);
        if has_exited {
            if let Some(terminal) = self.terminals.remove(&term_id)
                && let Some(resume_key) = terminal.resume_key
            {
                self.terminals_by_resume_key.remove(&resume_key);
            }
            return Ok(None);
        }
        let Some(terminal) = self.terminals.get_mut(&term_id) else {
            self.terminals_by_resume_key.remove(resume_key);
            return Ok(None);
        };
        if terminal.resume_key.is_none() {
            terminal.resume_key = Some(resume_key.to_string());
            self.terminals_by_resume_key
                .insert(resume_key.to_string(), term_id);
        }
        terminal.master.resize(PtySize {
            rows: dimension(params, "rows", 24),
            cols: dimension(params, "cols", 80),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let (data_method, exit_method) = notification_methods(params, term_id);
        let (history, exit_status) = {
            let mut output = terminal
                .output
                .lock()
                .map_err(|_| anyhow!("terminal output lock poisoned"))?;
            output.data_method = data_method;
            output.exit_method = exit_method;
            (BASE64.encode(&output.history), output.exit_status)
        };
        Ok(Some(json!({
            "term_id": term_id,
            "resumed": true,
            "history": history,
            "exit_status": exit_status,
        })))
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

    fn bind(&mut self, params: &Value) -> Result<Value> {
        let term_id = terminal_id(params)?;
        let resume_key = params
            .get("resume_key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .context("missing terminal resume key")?
            .to_string();
        let Some(terminal) = self.terminals.get_mut(&term_id) else {
            return Ok(Value::Null);
        };
        if params
            .get("notification_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            let (data_method, exit_method) = notification_methods(params, term_id);
            let mut output = terminal
                .output
                .lock()
                .map_err(|_| anyhow!("terminal output lock poisoned"))?;
            output.data_method = data_method;
            output.exit_method = exit_method;
        }
        if let Some(previous_key) = terminal.resume_key.replace(resume_key.clone()) {
            self.terminals_by_resume_key.remove(&previous_key);
        }
        self.terminals_by_resume_key.insert(resume_key, term_id);
        Ok(Value::Null)
    }

    fn attach(&mut self, params: &Value) -> Result<Value> {
        let term_id = terminal_id(params)?;
        let Some(terminal) = self.terminals.get_mut(&term_id) else {
            return Ok(json!({"attached": false}));
        };
        let (data_method, exit_method) = notification_methods(params, term_id);
        let exit_status = {
            let mut output = terminal
                .output
                .lock()
                .map_err(|_| anyhow!("terminal output lock poisoned"))?;
            output.data_method = data_method;
            output.exit_method = exit_method;
            output.exit_status
        };
        Ok(json!({"attached": true, "exit_status": exit_status}))
    }

    fn close(&mut self, params: &Value) -> Result<Value> {
        let term_id = terminal_id(params)?;
        if let Some(terminal) = self.terminals.remove(&term_id) {
            if let Some(resume_key) = terminal.resume_key.as_ref() {
                self.terminals_by_resume_key.remove(resume_key);
            }
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

fn notification_methods(params: &Value, term_id: u64) -> (String, String) {
    let notification_id = params
        .get("notification_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| term_id.to_string());
    (
        format!("Terminal::data:{notification_id}"),
        format!("Terminal::exit:{notification_id}"),
    )
}

fn terminal_data(term_id: u64, bytes: &[u8]) -> Value {
    json!({
        "term_id": term_id,
        "data": BASE64.encode(bytes),
    })
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
    use std::{sync::Arc, thread, time::Duration};

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{TerminalManager, is_external_agent_npm_prefix};
    use crate::fs_rpc::FsRpc;

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

    #[test]
    fn preserves_terminal_environment_except_external_agent_npm_prefix() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let opened = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": [
                        "-c",
                        "printf '%s|%s|%s' \"$AUTH_TOKEN\" \"$npm_config_prefix\" \"$PROJECT_ENV\""
                    ]
                },
                "env": {
                    "AUTH_TOKEN": "auth-token-is-present",
                    "npm_config_prefix": "/Users/zee/.npm-global",
                    "PROJECT_ENV": "project-setting-is-present"
                }
            }),
        )?;
        let term_id = opened["term_id"].as_u64().unwrap();

        wait_for_output(
            &terminals,
            term_id,
            b"auth-token-is-present|/Users/zee/.npm-global|project-setting-is-present",
        )
    }

    #[test]
    fn filters_external_agent_npm_prefix_from_spawned_terminal() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let opened = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": [
                        "-c",
                        "printf '%s|%s|%s' \"${npm_config_prefix-unset}\" \"${NPM_CONFIG_PREFIX-unset}\" \"$AUTH_TOKEN\""
                    ]
                },
                "env": {
                    "AUTH_TOKEN": "auth-token-is-present",
                    "npm_config_prefix": "/workspace/.config/zed/external_agents/registry/npx/claude-agent-acp",
                    "NPM_CONFIG_PREFIX": "/workspace/.config/zed/external_agents/registry/npx/codex-acp"
                }
            }),
        )?;
        let term_id = opened["term_id"].as_u64().unwrap();

        wait_for_output(&terminals, term_id, b"unset|unset|auth-token-is-present")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configures_frontend_url_bridge_in_spawned_terminal() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let opened = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": [
                        "-c",
                        "case \"$(command -v open)\" in */bin/open) printf 'open-shim|' ;; *) printf 'missing-shim|' ;; esac; case \"$ZED_WEB_OPEN_URL_SOCKET\" in */open-url.sock) printf 'socket-set' ;; *) printf 'socket-missing' ;; esac"
                    ]
                }
            }),
        )?;
        let term_id = opened["term_id"].as_u64().unwrap();

        wait_for_output(&terminals, term_id, b"open-shim|socket-set")
    }

    #[test]
    fn rewrites_virtual_paths_in_terminal_arguments_and_environment() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let canonical_root = root.path().canonicalize()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let opened = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": [
                        "-c",
                        "printf '%s|%s' \"$1\" \"$AUTH_PATH\"",
                        "auth-test",
                        "/workspace/.config/agent"
                    ]
                },
                "env": {
                    "AUTH_PATH": "/workspace/.config/credentials"
                }
            }),
        )?;
        let term_id = opened["term_id"].as_u64().unwrap();
        let expected = format!(
            "{}|{}",
            canonical_root.join(".config/agent").display(),
            canonical_root.join(".config/credentials").display()
        );

        for _ in 0..100 {
            let has_expected = terminals
                .terminals
                .get(&term_id)
                .unwrap()
                .output
                .lock()
                .unwrap()
                .history
                .windows(expected.len())
                .any(|window| window == expected.as_bytes());
            if has_expected {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }

        anyhow::bail!("terminal output did not contain rewritten virtual paths")
    }

    fn wait_for_output(
        terminals: &TerminalManager,
        term_id: u64,
        expected: &[u8],
    ) -> anyhow::Result<()> {
        for _ in 0..100 {
            let has_expected = terminals
                .terminals
                .get(&term_id)
                .unwrap()
                .output
                .lock()
                .unwrap()
                .history
                .windows(expected.len())
                .any(|window| window == expected);
            if has_expected {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }

        anyhow::bail!(
            "terminal output did not contain {:?}",
            String::from_utf8_lossy(expected)
        )
    }

    #[test]
    fn resumes_bound_terminal_with_same_id_and_history() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let opened = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": ["-c", "printf 'persistent-terminal-marker\\n'; sleep 30"]
                },
                "notification_id": "initial"
            }),
        )?;
        let term_id = opened["term_id"].as_u64().unwrap();

        let marker = b"persistent-terminal-marker";
        for _ in 0..100 {
            let has_marker = terminals
                .terminals
                .get(&term_id)
                .unwrap()
                .output
                .lock()
                .unwrap()
                .history
                .windows(marker.len())
                .any(|window| window == marker);
            if has_marker {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        terminals.dispatch(
            "Terminal::bind",
            &json!({
                "term_id": term_id,
                "resume_key": "workspace:1:terminal:9",
                "notification_id": "persisted-workspace:1:terminal:9"
            }),
        )?;
        let output = terminals
            .terminals
            .get(&term_id)
            .unwrap()
            .output
            .lock()
            .unwrap();
        assert_eq!(
            output.data_method,
            "Terminal::data:persisted-workspace:1:terminal:9"
        );
        assert_eq!(
            output.exit_method,
            "Terminal::exit:persisted-workspace:1:terminal:9"
        );
        drop(output);
        let resumed = terminals.dispatch(
            "Terminal::open",
            &json!({
                "resume_key": "workspace:1:terminal:9",
                "notification_id": "persisted-workspace:1:terminal:9"
            }),
        )?;
        assert_eq!(resumed["term_id"], term_id);
        assert_eq!(resumed["resumed"], true);
        let history = BASE64.decode(resumed["history"].as_str().unwrap())?;
        assert!(history.windows(marker.len()).any(|window| window == marker));

        let attached = terminals.dispatch(
            "Terminal::attach",
            &json!({
                "term_id": term_id,
                "notification_id": "reconnected-workspace:1:terminal:9"
            }),
        )?;
        assert_eq!(attached["attached"], true);
        let output = terminals
            .terminals
            .get(&term_id)
            .unwrap()
            .output
            .lock()
            .unwrap();
        assert_eq!(
            output.data_method,
            "Terminal::data:reconnected-workspace:1:terminal:9"
        );
        assert_eq!(
            output.exit_method,
            "Terminal::exit:reconnected-workspace:1:terminal:9"
        );
        drop(output);

        let missing = terminals.dispatch(
            "Terminal::attach",
            &json!({"term_id": term_id + 1, "notification_id": "missing"}),
        )?;
        assert_eq!(missing["attached"], false);
        terminals.dispatch("Terminal::close", &json!({"term_id": term_id}))?;
        Ok(())
    }

    #[test]
    fn does_not_resume_an_unbound_terminal_for_an_unrelated_key() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let unbound = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": ["-c", "sleep 30"]
                }
            }),
        )?;
        let unbound_id = unbound["term_id"].as_u64().unwrap();

        let keyed = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": ["-c", "sleep 30"]
                },
                "resume_key": "workspace:2:terminal:7"
            }),
        )?;
        let keyed_id = keyed["term_id"].as_u64().unwrap();

        assert_ne!(keyed_id, unbound_id);
        assert_eq!(keyed["resumed"], false);
        assert_eq!(
            terminals
                .terminals_by_resume_key
                .get("workspace:2:terminal:7"),
            Some(&keyed_id)
        );
        terminals.dispatch("Terminal::close", &json!({"term_id": unbound_id}))?;
        terminals.dispatch("Terminal::close", &json!({"term_id": keyed_id}))?;
        Ok(())
    }

    #[test]
    fn replaces_exited_terminal_during_restore() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel();
        let mut terminals = TerminalManager::new(fs, outgoing);
        let resume_key = "workspace:1:terminal:10";
        let opened = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": ["-c", "exit 0"]
                },
                "resume_key": resume_key,
                "notification_id": "initial"
            }),
        )?;
        let exited_term_id = opened["term_id"].as_u64().unwrap();

        for _ in 0..100 {
            let has_exited = terminals
                .terminals
                .get(&exited_term_id)
                .unwrap()
                .output
                .lock()
                .unwrap()
                .exit_status
                .is_some();
            if has_exited {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let restored = terminals.dispatch(
            "Terminal::open",
            &json!({
                "shell": {
                    "program": "/bin/sh",
                    "args": ["-c", "sleep 30"]
                },
                "resume_key": resume_key,
                "notification_id": "restored"
            }),
        )?;
        let restored_term_id = restored["term_id"].as_u64().unwrap();
        assert_ne!(restored_term_id, exited_term_id);
        assert_eq!(restored["resumed"], false);
        assert!(!terminals.terminals.contains_key(&exited_term_id));
        assert_eq!(
            terminals.terminals_by_resume_key.get(resume_key),
            Some(&restored_term_id)
        );

        terminals.dispatch("Terminal::close", &json!({"term_id": restored_term_id}))?;
        Ok(())
    }
}
