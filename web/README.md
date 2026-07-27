# Zed Web

This fork runs Zed's real GPUI workspace in a browser. The browser contains the
editor UI and WebGPU renderer; filesystem, SQLite, Git, terminals, language
servers, debuggers, extensions, and ACP agents run in the Rust backend.

The deployment is intentionally single-user. Collaboration, calls, channels,
and multi-user presence are outside its scope.

## Run The Published Image

```sh
mkdir -p workspace
ZED_WORKSPACE="$PWD/workspace" docker compose -f web/compose.yml up -d
```

Open `http://127.0.0.1:8090`. On first start, the server creates an access token
at `<workspace>/.zed/web-auth-token`. Use that token on the login page.

The same tested multi-architecture release is mirrored to Docker Hub after each
successful build. To use that registry instead, set:

```sh
ZED_WEB_IMAGE=docker.io/<dockerhub-user>/zed-web:latest \
ZED_WORKSPACE="$PWD/workspace" \
docker compose -f web/compose.yml up -d
```

To provide a stable token:

```sh
ZED_WEB_TOKEN='replace-with-a-long-secret' \
ZED_WORKSPACE=/absolute/path/to/project \
docker compose -f web/compose.yml up -d
```

The one image contains:

- `zed-web-server`, the native Rust backend
- `zed-extension-runtime`, the server-side Wasmtime extension host
- the complete Zed Web WASM frontend
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
