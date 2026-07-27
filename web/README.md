# Zed Web

This fork runs Zed's real GPUI workspace in a browser. The browser contains the
editor UI and WebGPU renderer; filesystem, SQLite, Git, terminals, language
servers, debuggers, extensions, and ACP agents run in the Rust backend.

The deployment is intentionally single-user. Collaboration, calls, channels,
and multi-user presence are outside its scope.

## Run The Published Image

Run the Docker Hub image directly:

```sh
mkdir -p workspace
docker run -d \
  --name zed-web \
  --restart unless-stopped \
  -p 8090:8090 \
  -v "$PWD/workspace:/workspace" \
  zee295/zed-web:1.13.0-web.1
docker exec zed-web sh -c \
  'until test -s /workspace/.zed/web-auth-token; do sleep 1; done; cat /workspace/.zed/web-auth-token'
```

Or use Compose from the repository:

```sh
mkdir -p workspace
ZED_WORKSPACE="$PWD/workspace" docker compose -f web/compose.yml up -d
```

Open `http://127.0.0.1:8090`. On first start, the server creates an access token
at `<workspace>/.zed/web-auth-token`. Use that token on the login page.

The same tested multi-architecture release is mirrored to Docker Hub after each
successful build. To use that registry instead, set:

```sh
ZED_WEB_IMAGE=docker.io/zee295/zed-web:1.13.0-web.1 \
ZED_WORKSPACE="$PWD/workspace" \
docker compose -f web/compose.yml up -d
```

To provide a stable token:

```sh
ZED_WEB_TOKEN='replace-with-a-long-secret' \
ZED_WORKSPACE=/absolute/path/to/project \
docker compose -f web/compose.yml up -d
```

## Docker Environment

Pass container variables with `docker run -e NAME=value` or under the Compose
service's `environment` section.

| Variable | Default | Purpose |
| --- | --- | --- |
| `ZED_WEB_TOKEN` | Generated once | Stable login token. Without it, the token is stored at `/workspace/.zed/web-auth-token`. |
| `ZED_WEB_PORT` | `8090` | Port listened to inside the container. The Docker port mapping must match it. |
| `ZED_WEB_WORKSPACE` | `/workspace` | Server workspace path. Mount persistent storage at the same path. |
| `ZED_WEB_RESTRICT_PATHS` | `false` | When `true`, prevents opening paths outside the workspace. |
| `ZED_WEB_SECURE_COOKIE` | `false` | Set to `true` when the public endpoint uses HTTPS. |
| `RUST_LOG` | `zed_web_server=info` | Rust backend log filter, for example `zed_web_server=debug`. |

Optional built-in agent and API proxy settings:

| Variable | Purpose |
| --- | --- |
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | Provider credential used only by the server. |
| `ZED_AGENT_API_KEY` | Generic provider credential fallback. |
| `ZED_AGENT_PROVIDER` | Force `openai`, `anthropic`, or `auto`. |
| `ZED_AGENT_MODEL` | Override the built-in agent model. |
| `ZED_AGENT_BASE_URL` | Override the built-in agent API endpoint. |
| `ZED_EXTERNAL_AGENTS` | JSON array of additional server-side agent commands. |

Compose also reads `ZED_WEB_IMAGE`, `ZED_WORKSPACE`, and the host-side
`ZED_WEB_PORT` before starting the container. These select the image, bind
mount source, and published host port respectively.

Example:

```sh
docker run -d \
  --name zed-web \
  --restart unless-stopped \
  -p 8090:8090 \
  -v "$PWD/workspace:/workspace" \
  -e ZED_WEB_TOKEN='replace-with-a-long-secret' \
  -e ZED_WEB_RESTRICT_PATHS=true \
  -e ZED_WEB_SECURE_COOKIE=false \
  zee295/zed-web:1.13.0-web.1
```

The one image contains:

- `zed-web-server`, the native Rust backend
- `zed-extension-runtime`, the server-side Wasmtime extension host
- the complete Zed Web WASM frontend
- Node.js 22 with npm and npx for extensions and ACP agents
- compressed fonts, icons, themes, prompts, images, and sounds
- Git, SSH, Node/npm, shells, and terminal runtime dependencies

The workspace is the only required volume. Persistent editor SQL, access token,
extension packages, terminals, agent state, and project data remain under that
mounted server filesystem.

## Reverse Proxy

WebAssembly threads require these response headers:

```text
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

The Rust server sets them directly. A reverse proxy must preserve them and
support WebSocket upgrades. Set `ZED_WEB_SECURE_COOKIE=true` when the public URL
uses HTTPS.

## Path Access

The server can open any path visible inside the container. Mount additional
server paths and open them by their container path. Set
`ZED_WEB_RESTRICT_PATHS=true` to restrict access to `/workspace`.

## Build From Source

Host prerequisites:

- Rust `1.95.0`
- Rust nightly with `rust-src`
- `wasm32-unknown-unknown`
- `wasm-bindgen-cli 0.2.120`
- Brotli, gzip, Perl, Clang, CMake, and Zed's Linux build dependencies

```sh
./web/build.sh
./web/run.sh /absolute/path/to/project
```

Or build the same single Docker image used by CI:

```sh
docker build -f web/Dockerfile -t zed-web:local .
docker run --rm -p 8090:8090 \
  -v /absolute/path/to/project:/workspace \
  zed-web:local
```

The production WASM build uses size optimization, one codegen unit, fat LTO,
shared memory, and maximum Brotli compression. A clean image build requires
substantial CPU, memory, and build time.

## Validation

```sh
cargo test -p zed_web_server --bin zed-web-server
cargo +nightly check -p zed_web_workspace \
  --target wasm32-unknown-unknown \
  -Z build-std=std,panic_abort
npm --prefix web/test run test:e2e
```

The existing browser suite may also be run from the development wrapper used
during initial porting.

## Updating From Upstream

Remotes must remain:

```text
origin    git@github.com:zee295/zed.git
upstream  https://github.com/zed-industries/zed.git
```

Test an upstream rebase without modifying the branch:

```sh
./web/sync-upstream.sh upstream/main
```

After reviewing the result:

```sh
./web/sync-upstream.sh --apply upstream/main
```

The helper rebases the small web patch stack instead of creating recurring
upstream merge commits. After each update, run server tests, the WASM check, a
production image build, and browser tests before pushing `zed-web`.

Release images are tagged with both the web branch revision and the upstream
base revision through `build-info.json`.
