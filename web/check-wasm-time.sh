#!/usr/bin/env bash

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
allowlist="$repo_root/web/wasm-std-instant.allowlist"
actual=$(mktemp)
trap 'rm -f "$actual"' EXIT

# std::time::Instant is valid in native and test-only code. Keep a normalized
# inventory of those uses so an upstream sync fails when it introduces a new
# occurrence anywhere in the web workspace dependency graph.
cargo tree \
    --manifest-path "$repo_root/Cargo.toml" \
    -p zed_web_workspace \
    --target wasm32-unknown-unknown \
    -e normal \
    --prefix none \
    --format '{p}' |
    sed -n "s|.*(\\($repo_root/crates/[^)]*\\)).*|\\1|p" |
    sort -u |
    while IFS= read -r crate_dir; do
        test -d "$crate_dir/src" || continue
        find "$crate_dir/src" -type f -name '*.rs' -print0 |
            while IFS= read -r -d '' source_file; do
                perl -0777 -ne '
                    while (/\buse\s+std(?:(?!;).)*?\bInstant\b(?:(?!;).)*?;/sg) {
                        $match = $&;
                        $match =~ s/\s+/ /g;
                        print "$ARGV:$match\n";
                    }
                    while (/\bstd::time::Instant\b/g) {
                        print "$ARGV:std::time::Instant\n";
                    }
                ' "$source_file"
            done
    done |
    sed "s|^$repo_root/||" |
    sort |
    uniq -c |
    sed -E 's/^ +//' >"$actual"

if ! diff -u "$allowlist" "$actual"; then
    cat >&2 <<'EOF'

Unexpected std::time::Instant usage was found in the web dependency graph.
It compiles for wasm32-unknown-unknown but Instant::now() panics in browsers.
Use web_time::Instant in runtime code. Update the allowlist only after proving
the occurrence is excluded from WASM by cfg or is test/fixture-only.
EOF
    exit 1
fi
