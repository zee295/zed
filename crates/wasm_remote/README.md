# wasm_remote

WebAssembly-side remote shims for Zed's browser client.

This crate lets a WASM frontend delegate real work to a backend `zed-server`
over a WebSocket. Instead of stubbing out `fs`, `git`, etc. as no-ops, the
frontend now forwards the trait calls and returns the backend's results to
the UI.

## What's implemented

- `RemoteClient` — JSON-RPC-over-WebSocket transport with request/response
  matching and an open/close lifecycle.
- `RemoteFs` — implements `fs::Fs`. File-system calls, including trash and
  restore, are serialized and sent to the backend.
- `RemoteGitRepository` — implements `git::repository::GitRepository`,
  including history, full commit diffs, blame, hooks, worktrees, and
  checkpoints.

## Protocol

The transport speaks a tiny JSON-RPC envelope:

```json
// request
{"id":1,"method":"Fs::read_dir","params":{"path":"."}}

// response
{"id":1,"result":{"entries":["src","Cargo.toml"]},"error":null}
```

The backend receives the `method` + `params`, dispatches to the real
implementation, and returns `result` or `error`.

## Usage

```rust
#[cfg(target_family = "wasm")]
fn start(cx: &mut App) {
    let client = wasm_remote::RemoteClient::connect("wss://my-zed-server/rpc")
        .expect("failed to connect");
    let fs: Arc<dyn fs::Fs> = Arc::new(wasm_remote::RemoteFs::new(client));

    // Pass `fs` into Project::local / Project::remote instead of RealFs.
}
```

## Next steps

- Reuse the existing `rpc::Peer` / `proto::Envelope` path for projects where
  the backend is already a Zed server; keep this lightweight JSON-RPC crate
  for standalone / custom backends.
- Port collaboration and extension-host event contracts onto browser-safe
  transports.
