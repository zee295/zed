# Zed Web

Run Zed's GPUI editor in a browser with a Rust backend. The browser provides
the editor UI and WebGPU renderer; filesystem access, SQLite, Git, terminals,
language servers, debuggers, extensions, and ACP agents run in the container.

**Project source and issue tracking:**
[github.com/zee295/zed/tree/zed-web](https://github.com/zee295/zed/tree/zed-web)

> **Preview status:** This is an independent web port, not an official Zed web
> release. Automated checks cover the Rust backend, WASM compilation, core
> browser flows, and linux/amd64 and linux/arm64 images. The complete desktop
> feature surface and long-running production deployments have not been
> exhaustively tested. Keep backups, pin a versioned image tag, and expect
> web-specific issues.

## Run

```sh
mkdir -p workspace
docker run -d \
  --name zed-web \
  --restart unless-stopped \
  -p 8090:8090 \
  -v "$PWD/workspace:/workspace" \
  zee295/zed-web:1.13.0-web.18

docker exec zed-web sh -c \
  'until test -s /workspace/.zed/web-auth-token; do sleep 1; done; cat /workspace/.zed/web-auth-token'
```

Open `http://127.0.0.1:8090` and enter the printed access token.

The `/workspace` mount stores project files, editor state, installed
extensions, terminal sessions, agent state, and the generated authentication
token. Back up this volume.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `ZED_WEB_TOKEN` | Generated once | Set a stable login token instead of reading the generated token from the volume. |
| `ZED_WEB_PORT` | `8090` | Port listened to inside the container. |
| `ZED_WEB_WORKSPACE` | `/workspace` | Workspace path inside the container. |
| `ZED_WEB_RESTRICT_PATHS` | `false` | Restrict file access to the workspace when set to `true`. |
| `ZED_WEB_SECURE_COOKIE` | `false` | Set to `true` behind a public HTTPS endpoint. |
| `RUST_LOG` | `zed_web_server=info` | Rust backend log filter. |

The image includes Node.js 22 with npm/npx for extensions and ACP agents.
Provider credentials such as `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` remain on
the server when passed as container environment variables.

Ollama, llama.cpp, and LM Studio requests also run through the Rust server. Use
`ZED_WEB_OLLAMA_URL`, `ZED_WEB_LLAMA_CPP_URL`, or `ZED_WEB_LM_STUDIO_URL` to
point the container at the model service. For a service on the Docker host,
add `--add-host=host.docker.internal:host-gateway` and use a URL such as
`http://host.docker.internal:11434`.

## Large Repositories

An `ENOSPC` error from `fs.watch` means the Docker host's shared Linux inotify
quota is exhausted, not that the disk is full. For large repositories, set
`fs.inotify.max_user_instances=1024`,
`fs.inotify.max_user_watches=524288`, and
`fs.inotify.max_queued_events=65536` on the host, then restart affected agents
or the container.

## Reverse Proxy

Preserve WebSocket upgrades and these response headers:

```text
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Set `ZED_WEB_SECURE_COOKIE=true` when the public endpoint uses HTTPS. Do not
expose the container directly to the public internet without authentication,
TLS, and appropriate host-level access controls.

## Tags And Platforms

- Pin release tags such as `1.13.0-web.18` for reproducible deployments.
- `latest` tracks the newest successful `zed-web` branch build and can change
  without a release.
- Published release images support `linux/amd64` and `linux/arm64`.

Full documentation, Compose configuration, build instructions, and release
notes are available in the
[GitHub repository](https://github.com/zee295/zed/tree/zed-web).
