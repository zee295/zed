#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixture="$(mktemp)"
trap 'rm -f "${fixture}"' EXIT

cat > "${fixture}" <<'EOF'
const oldBuffer = new ArrayBuffer(8);
const newBuffer = new ArrayBuffer(16);
let returnOldBuffer = true;
const wasm = {
    memory: {
        get buffer() {
            if (returnOldBuffer) {
                returnOldBuffer = false;
                return oldBuffer;
            }
            return newBuffer;
        },
    },
};
function memoryAccessor() {
    const ret = wasm.memory;
    return ret;
}
let cachedDataViewMemory0 = new DataView(oldBuffer);
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer !== wasm.memory.buffer) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}
getDataViewMemory0().setInt32(8, 42, true);
if (new DataView(newBuffer).getInt32(8, true) !== 42) {
    throw new Error("DataView write was not retried after memory growth");
}
EOF

"${script_dir}/patch-wasm-bindgen-memory.sh" "${fixture}"
node "${fixture}"
"${script_dir}/patch-wasm-bindgen-memory.sh" "${fixture}"

grep -Fq 'function accessDataViewMemory0(method, byteOffset, byteWidth, ...args) {' "${fixture}"
[[ "$(grep -Fc 'function accessDataViewMemory0(method, byteOffset, byteWidth, ...args) {' "${fixture}")" == "1" ]]
