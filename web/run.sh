#!/usr/bin/env bash
set -euo pipefail

web_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace="${1:-${PWD}}"
if [[ $# -gt 0 ]]; then
    shift
fi

server="${ZED_WEB_SERVER:-${web_dir}/dist/bin/zed-web-server}"
static_root="${ZED_WEB_STATIC_ROOT:-${web_dir}/dist/static}"

if [[ ! -x "${server}" ]]; then
    echo "Zed Web is not built. Run ./web/build.sh first." >&2
    exit 1
fi

export ZED_EXTENSION_RUNTIME="${ZED_EXTENSION_RUNTIME:-${web_dir}/dist/bin/zed-extension-runtime}"
exec "${server}" \
    "${workspace}" \
    "${static_root}" \
    --host "${ZED_WEB_HOST:-127.0.0.1}" \
    --port "${ZED_WEB_PORT:-8090}" \
    "$@"
