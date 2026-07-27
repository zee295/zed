#!/usr/bin/env bash
set -euo pipefail

workspace="${ZED_WEB_WORKSPACE:-/workspace}"
port="${ZED_WEB_PORT:-8090}"

mkdir -p "${workspace}"

args=(
    /usr/local/bin/zed-web-server
    "${workspace}"
    /opt/zed-web/static
    --host 0.0.0.0
    --port "${port}"
)

if [[ "${ZED_WEB_SECURE_COOKIE:-false}" == "true" ]]; then
    args+=(--secure-cookie)
fi
if [[ "${ZED_WEB_RESTRICT_PATHS:-false}" == "true" ]]; then
    args+=(--restrict-paths)
elif [[ "${ZED_WEB_RESTRICT_PATHS:-}" == "false" ]]; then
    args+=(--no-restrict-paths)
fi

exec "${args[@]}" "$@"
