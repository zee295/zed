#!/usr/bin/env bash
set -euo pipefail

web_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "${web_dir}/.." && pwd)"
dist_dir="${ZED_WEB_DIST_DIR:-${web_dir}/dist}"
static_dir="${dist_dir}/static"
native_target="${ZED_WEB_NATIVE_TARGET:-${repo_dir}/target/web-native}"
wasm_target="${CARGO_TARGET_DIR:-${repo_dir}/target/web-wasm}"
profile="${ZED_WEB_PROFILE:-web-release}"
stable_toolchain="${RUST_STABLE_TOOLCHAIN:-1.95.0}"
nightly_toolchain="${RUST_NIGHTLY_TOOLCHAIN:-nightly}"

revision() {
    local fallback="$1"
    shift
    git -C "${repo_dir}" "$@" 2>/dev/null || printf '%s' "${fallback}"
}

web_revision="$(revision "${WEB_REVISION:-unknown}" rev-parse HEAD)"
upstream_revision="$(revision "${UPSTREAM_REVISION:-unknown}" merge-base HEAD upstream/main)"

rm -rf "${dist_dir}"
mkdir -p "${dist_dir}/bin" "${static_dir}"
install -m 0644 "${web_dir}/static/workspace.html" "${static_dir}/workspace.html"

rustup run "${stable_toolchain}" "${CARGO:-cargo}" build \
    --manifest-path "${repo_dir}/Cargo.toml" \
    --target-dir "${native_target}" \
    --release \
    -p extension_runtime_cli \
    -p zed_web_server

install -m 0755 \
    "${native_target}/release/zed-web-server" \
    "${dist_dir}/bin/zed-web-server"
install -m 0755 \
    "${native_target}/release/zed-extension-runtime" \
    "${dist_dir}/bin/zed-extension-runtime"

export CARGO_TARGET_DIR="${wasm_target}"
export RUSTFLAGS='--cfg getrandom_backend="wasm_js" -C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=4294967296 -C link-arg=--export=__heap_base -C link-arg=--export=__stack_pointer -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__wasm_call_ctors'

rustup run "${nightly_toolchain}" cargo build \
    --manifest-path "${repo_dir}/Cargo.toml" \
    -p zed_web_workspace \
    --target wasm32-unknown-unknown \
    --profile "${profile}" \
    -Z build-std=std,panic_abort

wasm-bindgen \
    --target web \
    --no-typescript \
    --out-dir "${static_dir}" \
    "${wasm_target}/wasm32-unknown-unknown/${profile}/zed_web_workspace.wasm"
"${web_dir}/scripts/patch-wasm-bindgen-memory.sh" \
    "${static_dir}/zed_web_workspace.js"

COPYFILE_DISABLE=1 tar -C "${repo_dir}/assets" \
    --exclude='._*' \
    --exclude='.DS_Store' \
    -cf "${static_dir}/zed-assets.tar" \
    fonts icons images themes sounds prompts

printf '{\n  "web_revision": "%s",\n  "upstream_revision": "%s"\n}\n' \
    "${web_revision}" \
    "${upstream_revision}" \
    > "${static_dir}/build-info.json"

wasm="${static_dir}/zed_web_workspace_bg.wasm"
raw="$(wc -c < "${wasm}" | tr -d ' ')"
gzip_size="$(gzip -9 -c "${wasm}" | wc -c | tr -d ' ')"
for asset in \
    "${static_dir}/workspace.html" \
    "${static_dir}/zed_web_workspace.js" \
    "${static_dir}/zed-assets.tar" \
    "${wasm}"; do
    gzip -9 -c "${asset}" > "${asset}.gz"
    if command -v brotli >/dev/null 2>&1; then
        brotli -q "${BROTLI_QUALITY:-11}" -f -o "${asset}.br" "${asset}"
    fi
done

brotli_size="unavailable"
if [[ -f "${wasm}.br" ]]; then
    brotli_size="$(wc -c < "${wasm}.br" | tr -d ' ')"
fi
printf 'Zed Web WASM raw:    %s bytes\n' "${raw}"
printf 'Zed Web WASM gzip:   %s bytes\n' "${gzip_size}"
printf 'Zed Web WASM brotli: %s bytes\n' "${brotli_size}"
printf 'Distribution:        %s\n' "${dist_dir}"
