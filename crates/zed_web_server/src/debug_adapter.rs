use std::{
    env,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use flate2::read::GzDecoder;
use serde_json::Value;
use tar::Archive;
use tokio::{
    io::{AsyncWriteExt as _, copy},
    net::TcpStream,
    process::Command as AsyncCommand,
};

pub fn resolve(program: &str, args: Vec<String>) -> Result<(PathBuf, Vec<String>)> {
    if program != "zed-web-debug-adapter" {
        return Ok((PathBuf::from(program), args));
    }
    let (adapter, adapter_args) = args
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("missing host debug adapter name"))?;
    let mut adapter_args = adapter_args.to_vec();
    match adapter.as_str() {
        "debugpy" | "javascript" | "delve" => {
            let mut args = vec!["__debug-adapter-proxy".to_string(), adapter.to_string()];
            args.append(&mut adapter_args);
            Ok((env::current_exe()?, args))
        }
        "gdb" => {
            let executable = find_executable("gdb")
                .ok_or_else(|| anyhow::anyhow!("gdb is not installed on the backend host"))?;
            if !adapter_args.iter().any(|arg| arg == "-i=dap") {
                adapter_args.insert(0, "-i=dap".to_string());
            }
            Ok((executable, adapter_args))
        }
        "lldb" => {
            let executable = find_executable("lldb-dap")
                .or_else(|| {
                    let output = Command::new("xcrun")
                        .args(["-f", "lldb-dap"])
                        .output()
                        .ok()?;
                    output.status.success().then(|| {
                        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
                    })
                })
                .ok_or_else(|| anyhow::anyhow!("lldb-dap is not installed on the backend host"))?;
            Ok((executable, adapter_args))
        }
        _ => bail!("unsupported host debug adapter: {adapter}"),
    }
}

pub async fn run_proxy(args: &[String]) -> Result<()> {
    let (adapter, extra) = args
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("missing debug adapter name"))?;
    let root = env::current_dir()?.join(".zed/debug-adapters");
    tokio::fs::create_dir_all(&root).await?;
    let port = reserve_port()?;
    let mut command = match adapter.as_str() {
        "debugpy" => debugpy_command(&root, port, extra).await?,
        "javascript" => javascript_command(&root, port, extra).await?,
        "delve" => delve_command(&root, port, extra).await?,
        _ => bail!("unsupported proxied debug adapter: {adapter}"),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().context("starting debug adapter")?;
    if let Some(mut output) = child.stdout.take() {
        tokio::spawn(async move {
            let mut stderr = tokio::io::stderr();
            copy(&mut output, &mut stderr).await.ok();
        });
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        if let Some(status) = child.try_wait()? {
            bail!("debug adapter exited before accepting connections ({status})");
        }
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("connecting to debug adapter"),
        }
    };
    let (mut read, mut write) = stream.split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = copy(&mut stdin, &mut write) => {
            result?;
            write.shutdown().await?;
            copy(&mut read, &mut stdout).await?;
        }
        result = copy(&mut read, &mut stdout) => {
            result?;
            stdout.flush().await?;
        }
    }
    child.start_kill().ok();
    child.wait().await.ok();
    Ok(())
}

async fn debugpy_command(root: &Path, port: u16, extra: &[String]) -> Result<AsyncCommand> {
    let install = root.join("debugpy");
    let python = if cfg!(windows) {
        install.join("Scripts/python.exe")
    } else {
        install.join("bin/python")
    };
    let lock = lock(root, "debugpy")?;
    if !python.exists() {
        let host_python = find_executable("python3")
            .or_else(|| find_executable("python"))
            .ok_or_else(|| anyhow::anyhow!("Python is required for the Debugpy adapter"))?;
        run(
            Command::new(host_python).args(["-m", "venv"]).arg(&install),
            "creating Debugpy environment",
        )?;
    }
    let marker = install.join(".installed");
    if !marker.exists() {
        run(
            Command::new(&python).args([
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-input",
                "debugpy",
            ]),
            "installing Debugpy",
        )?;
        fs::write(marker, b"debugpy\n")?;
    }
    fs2::FileExt::unlock(&lock)?;
    let mut command = AsyncCommand::new(python);
    command.args([
        "-m",
        "debugpy.adapter",
        "--host=127.0.0.1",
        &format!("--port={port}"),
    ]);
    command.args(extra);
    Ok(command)
}

async fn javascript_command(root: &Path, port: u16, extra: &[String]) -> Result<AsyncCommand> {
    let node = find_executable("node")
        .ok_or_else(|| anyhow::anyhow!("Node.js is required for the JavaScript debug adapter"))?;
    let install = root.join("javascript");
    let lock = lock(root, "javascript")?;
    let response = reqwest::Client::new()
        .get("https://api.github.com/repos/microsoft/vscode-js-debug/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "zedweb-light")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let response: Value = serde_json::from_slice(&response)?;
    let tag = response["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("JavaScript adapter release has no tag"))?;
    let version = install.join(safe_component(tag));
    if !version.exists() {
        let asset = response["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|asset| {
                asset["name"].as_str().is_some_and(|name| {
                    name.starts_with("js-debug-dap-") && name.ends_with(".tar.gz")
                })
            })
            .and_then(|asset| asset["browser_download_url"].as_str())
            .ok_or_else(|| anyhow::anyhow!("JavaScript adapter release archive is missing"))?;
        let archive = reqwest::Client::new()
            .get(asset)
            .header("User-Agent", "zedweb-light")
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let staging = install.join(format!(".staging-{}", std::process::id()));
        fs::remove_dir_all(&staging).ok();
        fs::create_dir_all(&staging)?;
        Archive::new(GzDecoder::new(archive.as_ref())).unpack(&staging)?;
        let entries = fs::read_dir(&staging)?.collect::<io::Result<Vec<_>>>()?;
        let source = if entries.len() == 1 && entries[0].file_type()?.is_dir() {
            entries[0].path()
        } else {
            staging.clone()
        };
        fs::create_dir_all(&install)?;
        fs::rename(&source, &version)?;
        fs::remove_dir_all(&staging).ok();
    }
    fs2::FileExt::unlock(&lock)?;
    let script = [
        version.join("src/dapDebugServer.js"),
        version.join("js-debug/src/dapDebugServer.js"),
    ]
    .into_iter()
    .find(|path| path.exists())
    .ok_or_else(|| anyhow::anyhow!("JavaScript debug adapter entry point is missing"))?;
    let mut command = AsyncCommand::new(node);
    command
        .arg(script)
        .args([port.to_string(), "127.0.0.1".to_string()]);
    command.args(extra);
    Ok(command)
}

async fn delve_command(root: &Path, port: u16, extra: &[String]) -> Result<AsyncCommand> {
    let binary = if let Some(system) = find_executable("dlv") {
        system
    } else {
        let bin = root
            .join("delve/bin")
            .join(if cfg!(windows) { "dlv.exe" } else { "dlv" });
        let lock = lock(root, "delve")?;
        if !bin.exists() {
            let go = find_executable("go")
                .ok_or_else(|| anyhow::anyhow!("Go is required to install Delve"))?;
            fs::create_dir_all(bin.parent().expect("Delve bin parent"))?;
            let mut command = Command::new(go);
            command
                .args(["install", "github.com/go-delve/delve/cmd/dlv@latest"])
                .env("GOBIN", bin.parent().expect("Delve bin parent"));
            run(&mut command, "installing Delve")?;
        }
        fs2::FileExt::unlock(&lock)?;
        bin
    };
    let mut command = AsyncCommand::new(binary);
    command.args(["dap", &format!("--listen=127.0.0.1:{port}")]);
    command.args(extra);
    Ok(command)
}

fn lock(root: &Path, name: &str) -> Result<File> {
    fs::create_dir_all(root)?;
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .open(root.join(format!("{name}.lock")))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn run(command: &mut Command, description: &str) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| description.to_string())?;
    if !status.success() {
        bail!("{description} failed with {status}");
    }
    Ok(())
}

fn reserve_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        candidate.is_file().then_some(candidate)
    })
}
