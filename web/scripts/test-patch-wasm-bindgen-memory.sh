#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixture="$(mktemp)"
trap 'rm -f "${fixture}"' EXIT

cat > "${fixture}" <<'EOF'
const buffer = new SharedArrayBuffer(8, { maxByteLength: 16 });
const wasm = {
    memory: {
        get buffer() {
            return buffer;
        },
    },
};
function memoryAccessor() {
    const ret = wasm.memory;
    return ret;
}
let cachedDataViewMemory0 = new DataView(buffer, 0, 8);
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer !== wasm.memory.buffer) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}
buffer.grow(16);
getDataViewMemory0().setInt32(8, 42, true);
if (new DataView(buffer).getInt32(8, true) !== 42) {
    throw new Error("DataView write was not retried after in-place memory growth");
}
EOF

"${script_dir}/patch-wasm-bindgen-memory.sh" "${fixture}"
node "${fixture}"
"${script_dir}/patch-wasm-bindgen-memory.sh" "${fixture}"

grep -Fq 'function accessDataViewMemory0(method, byteOffset, byteWidth, ...args) {' "${fixture}"
[[ "$(grep -Fc 'function accessDataViewMemory0(method, byteOffset, byteWidth, ...args) {' "${fixture}")" == "1" ]]
grep -Fq 'cachedDataViewMemory0.byteLength !== currentBuffer.byteLength' "${fixture}"
