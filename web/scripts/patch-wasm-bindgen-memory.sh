#!/usr/bin/env bash
set -euo pipefail

js_file="${1:?usage: patch-wasm-bindgen-memory.sh <wasm-bindgen-js>}"

accessor='const ret = wasm.memory || globalThis.__wbgSharedMemory;'
if ! grep -Fq "${accessor}" "${js_file}"; then
    match_count="$(grep -Fc 'const ret = wasm.memory;' "${js_file}" || true)"
    if [[ "${match_count}" != "1" ]]; then
        echo "expected one wasm-bindgen memory accessor in ${js_file}, found ${match_count}" >&2
        exit 1
    fi
    perl -0pi -e \
        's/const ret = wasm\.memory;/const ret = wasm.memory || globalThis.__wbgSharedMemory;/' \
        "${js_file}"
fi

memory_stash='globalThis.__wbgSharedMemory = memory;'
if grep -Fq 'memory: memory || new WebAssembly.Memory(' "${js_file}"; then
    perl -0pi -e \
        's/memory: memory \|\| new WebAssembly\.Memory\(/memory: memory || (globalThis.__wbgSharedMemory = new WebAssembly.Memory(/' \
        "${js_file}"
    perl -0pi -e \
        's/(globalThis\.__wbgSharedMemory = new WebAssembly\.Memory\(\{[^}]+\}\))/$1)/' \
        "${js_file}"
fi

if grep -Fq 'memory: memory || (globalThis.__wbgSharedMemory' "${js_file}" &&
    ! grep -Fq "${memory_stash}" "${js_file}"; then
    perl -0pi -e \
        's/(memory: memory \|\| \(globalThis\.__wbgSharedMemory = new WebAssembly\.Memory\(\{[^}]+\}\)\),\n    \};)/$1\n    if (memory) { globalThis.__wbgSharedMemory = memory; }/' \
        "${js_file}"
fi

if grep -Fq 'shared:true' "${js_file}" &&
    ! grep -Fq 'globalThis.__zedCallCtors = () =>' "${js_file}"; then
    match_count="$(grep -Fc 'wasm.__wbindgen_start(thread_stack_size);' "${js_file}" || true)"
    if [[ "${match_count}" != "1" ]]; then
        echo "expected one wasm-bindgen start call in ${js_file}, found ${match_count}" >&2
        exit 1
    fi
    perl -0pi -e \
        's/    wasm\.__wbindgen_start\(thread_stack_size\);/    globalThis.__zedCallCtors = () => {\n        if (typeof wasm.__wasm_call_ctors === '"'"'function'"'"') {\n            wasm.__wasm_call_ctors();\n        }\n    };\n    wasm.__wbindgen_start(thread_stack_size);/' \
        "${js_file}"
fi

data_view_accessor='function accessDataViewMemory0(method, byteOffset, byteWidth, ...args) {'
if grep -Fq 'function getDataViewMemory0() {' "${js_file}" &&
    ! grep -Fq "${data_view_accessor}" "${js_file}"; then
    perl -0pi -e '
        s~if \(cachedDataViewMemory0 === null \|\| cachedDataViewMemory0\.buffer !== wasm\.memory\.buffer\) \{\n        cachedDataViewMemory0 = new DataView\(wasm\.memory\.buffer\);~const currentBuffer = wasm.memory.buffer;\n    if (cachedDataViewMemory0 === null ||\n        cachedDataViewMemory0.buffer !== currentBuffer ||\n        cachedDataViewMemory0.byteLength !== currentBuffer.byteLength) {\n        cachedDataViewMemory0 = new DataView(currentBuffer);~;
        s/getDataViewMemory0\(\)\.getInt32\(/getDataViewInt32(/g;
        s/getDataViewMemory0\(\)\.setInt32\(/setDataViewInt32(/g;
        s/getDataViewMemory0\(\)\.setFloat64\(/setDataViewFloat64(/g;
        s~(function getDataViewMemory0\(\) \{.*?\n\})~$1\n\nfunction accessDataViewMemory0(method, byteOffset, byteWidth, ...args) {\n    for (let attempt = 0; ; attempt++) {\n        try {\n            return getDataViewMemory0()[method](byteOffset, ...args);\n        } catch (error) {\n            const currentBuffer = wasm.memory.buffer;\n            const addressIsValid = Number.isInteger(byteOffset) &&\n                byteOffset >= 0 && byteOffset + byteWidth <= currentBuffer.byteLength;\n            if (!(error instanceof RangeError) || !addressIsValid || attempt >= 2) {\n                throw error;\n            }\n            cachedDataViewMemory0 = new DataView(currentBuffer);\n        }\n    }\n}\n\nfunction getDataViewInt32(byteOffset, littleEndian) {\n    return accessDataViewMemory0("getInt32", byteOffset, 4, littleEndian);\n}\n\nfunction setDataViewInt32(byteOffset, value, littleEndian) {\n    return accessDataViewMemory0("setInt32", byteOffset, 4, value, littleEndian);\n}\n\nfunction setDataViewFloat64(byteOffset, value, littleEndian) {\n    return accessDataViewMemory0("setFloat64", byteOffset, 8, value, littleEndian);\n}~s;
    ' "${js_file}"
fi

grep -Fq "${accessor}" "${js_file}"
if grep -Fq 'function getDataViewMemory0() {' "${js_file}"; then
    grep -Fq "${data_view_accessor}" "${js_file}"
    grep -Fq 'cachedDataViewMemory0.byteLength !== currentBuffer.byteLength' "${js_file}"
    ! grep -Fq 'getDataViewMemory0().getInt32(' "${js_file}"
    ! grep -Fq 'getDataViewMemory0().setInt32(' "${js_file}"
    ! grep -Fq 'getDataViewMemory0().setFloat64(' "${js_file}"
fi
if grep -Fq 'shared:true' "${js_file}"; then
    grep -Fq 'memory: memory || (globalThis.__wbgSharedMemory' "${js_file}"
    grep -Fq "${memory_stash}" "${js_file}"
    grep -Fq 'globalThis.__zedCallCtors = () =>' "${js_file}"
fi
