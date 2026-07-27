#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
branch="${ZED_WEB_BRANCH:-zed-web}"
scratch="${TMPDIR:-/tmp}/zed-web-sync-$$"
apply=false
upstream_ref="upstream/main"

for argument in "$@"; do
    case "${argument}" in
        --apply) apply=true ;;
        --help|-h)
            cat <<'EOF'
Usage: ./web/sync-upstream.sh [--apply] [upstream-ref]

Without --apply, tests the rebase in a temporary worktree. The default ref is
upstream/main; a release tag or exact commit may be supplied instead.
EOF
            exit 0
            ;;
        -*) echo "unknown option: ${argument}" >&2; exit 2 ;;
        *) upstream_ref="${argument}" ;;
    esac
done

git -C "${repo}" fetch --tags upstream
target="$(git -C "${repo}" rev-parse --verify "${upstream_ref}^{commit}")"
base="$(git -C "${repo}" merge-base "${branch}" "${target}")"
patch_count="$(git -C "${repo}" rev-list --count "${base}..${branch}")"
upstream_count="$(git -C "${repo}" rev-list --count "${base}..${target}")"

printf 'Rebasing %s web commits over %s (%s upstream commits)\n' \
    "${patch_count}" \
    "$(git -C "${repo}" rev-parse --short "${target}")" \
    "${upstream_count}"

if [[ "${upstream_count}" = "0" ]]; then
    echo "Already up to date."
    exit 0
fi

cleanup() {
    git -C "${repo}" worktree remove --force "${scratch}" 2>/dev/null || true
    git -C "${repo}" branch -D zed-web-sync-tmp -q 2>/dev/null || true
}
trap cleanup EXIT

git -C "${repo}" worktree add -q -b zed-web-sync-tmp "${scratch}" "${branch}"
if ! git -C "${scratch}" rebase --onto "${target}" "${base}" zed-web-sync-tmp; then
    echo "The upstream rebase has conflicts; the primary worktree was not changed." >&2
    exit 1
fi

if [[ "${apply}" != "true" ]]; then
    echo "Dry run passed. Re-run with --apply to update ${branch}."
    exit 0
fi
if [[ -n "$(git -C "${repo}" status --porcelain)" ]]; then
    echo "Cannot apply with an uncommitted worktree." >&2
    exit 1
fi

cleanup
trap - EXIT
git -C "${repo}" rebase --onto "${target}" "${base}" "${branch}"
echo "Rebase complete. Run the validation commands in web/README.md."
