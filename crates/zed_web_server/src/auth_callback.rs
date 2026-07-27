use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::TcpStream,
    time::timeout,
};
use url::Host;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn relay(params: &Value) -> Result<Value> {
    let callback = params
        .get("url")
        .and_then(Value::as_str)
        .context("missing callback URL")?;
    relay_url(callback).await?;
    Ok(Value::Null)
}

async fn relay_url(callback: &str) -> Result<()> {
    let callback = url::Url::parse(callback).context("invalid callback URL")?;
    if callback.scheme() != "http" {
        bail!("callback URL must use http");
    }

    let (connect_host, host_header) = match callback.host().context("callback URL has no host")? {
        Host::Domain(host) if host.eq_ignore_ascii_case("localhost") => {
            ("localhost".to_string(), "localhost".to_string())
        }
        Host::Ipv4(address) if address.is_loopback() => (address.to_string(), address.to_string()),
        Host::Ipv6(address) if address.is_loopback() => {
            (address.to_string(), format!("[{address}]"))
        }
        _ => bail!("callback URL must target localhost"),
    };
    let port = callback
        .port()
        .context("callback URL must include the listener port")?;

    let mut stream = timeout(
        CALLBACK_TIMEOUT,
        TcpStream::connect((connect_host.as_str(), port)),
    )
    .await
    .context("timed out connecting to the authentication listener")?
    .context("authentication listener is not reachable")?;
    let mut target = callback.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = callback.query() {
        target.push('?');
        target.push_str(query);
    }
    let host_header = format!("{host_header}:{port}");
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nAccept: text/html,*/*\r\nConnection: close\r\n\r\n"
    );
    timeout(CALLBACK_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .context("timed out forwarding the authentication callback")??;

    let mut response = BufReader::new(stream);
    let mut status = String::new();
    timeout(CALLBACK_TIMEOUT, response.read_line(&mut status))
        .await
        .context("timed out waiting for the authentication listener")??;
    if !status.starts_with("HTTP/1.1 2")
        && !status.starts_with("HTTP/1.1 3")
        && !status.starts_with("HTTP/1.0 2")
        && !status.starts_with("HTTP/1.0 3")
    {
        bail!("authentication listener rejected the callback");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn relays_callback_to_loopback_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let receiver = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 1024];
            let count = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&request[..count]).into_owned()
        });

        relay_url(&format!(
            "http://127.0.0.1:{port}/auth/callback?code=test&state=expected"
        ))
        .await
        .unwrap();

        let request = receiver.await.unwrap();
        assert!(request.starts_with("GET /auth/callback?code=test&state=expected HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn rejects_non_loopback_callbacks() {
        assert!(
            relay_url("http://example.com:1455/auth/callback")
                .await
                .unwrap_err()
                .to_string()
                .contains("localhost")
        );
        assert!(
            relay_url("https://localhost:1455/auth/callback")
                .await
                .unwrap_err()
                .to_string()
                .contains("must use http")
        );
    }
}
