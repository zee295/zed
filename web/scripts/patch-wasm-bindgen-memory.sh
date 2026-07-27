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

if ! grep -Fq 'globalThis.__zedCallCtors = () =>' "${js_file}"; then
    match_count="$(grep -Fc 'wasm.__wbindgen_start(thread_stack_size);' "${js_file}" || true)"
    if [[ "${match_count}" != "1" ]]; then
        echo "expected one wasm-bindgen start call in ${js_file}, found ${match_count}" >&2
        exit 1
    fi
    perl -0pi -e \
        's/    wasm\.__wbindgen_start\(thread_stack_size\);/    globalThis.__zedCallCtors = () => {\n        if (typeof wasm.__wasm_call_ctors === '"'"'function'"'"') {\n            wasm.__wasm_call_ctors();\n        }\n    };\n    wasm.__wbindgen_start(thread_stack_size);/' \
        "${js_file}"
fi

grep -Fq "${accessor}" "${js_file}"
if grep -Fq 'shared:true' "${js_file}"; then
    grep -Fq 'memory: memory || (globalThis.__wbgSharedMemory' "${js_file}"
    grep -Fq "${memory_stash}" "${js_file}"
    grep -Fq 'globalThis.__zedCallCtors = () =>' "${js_file}"
fi
