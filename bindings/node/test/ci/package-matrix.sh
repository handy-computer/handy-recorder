#!/usr/bin/env bash
# Runs the smoke test for each installer:runtime pair (by default every
# package manager and every runtime once), then fails if any failed.
#
#   package-matrix.sh <tarball, or its folder> <work dir> [installer:runtime...]
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
package=$1
work=$2
shift 2
combos=("$@")
[ ${#combos[@]} -gt 0 ] || combos=(npm:node npm:deno pnpm:node bun:bun yarn:node)
mkdir -p "$work"

group() { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::group::$1" || echo "== $1"; }
endgroup() { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::endgroup::" || true; }

results=()
failed=0
record() {
    results+=("$1 $2")
    [ "$1" = ok ] || failed=$((failed + 1))
}

installed=" "
broken=" "
for combo in "${combos[@]}"; do
    installer=${combo%%:*}
    runtime=${combo#*:}
    dir="$work/$installer"

    # Install once per package manager.
    if [[ $installed != *" $installer "* && $broken != *" $installer "* ]]; then
        group "install with $installer"
        if "$here/install-package.sh" "$installer" "$package" "$dir"; then
            installed+="$installer "
        else
            broken+="$installer "
            record FAIL "$installer: install"
        fi
        endgroup
    fi
    [[ $installed == *" $installer "* ]] || continue

    case $runtime in
        node) run=(node) ;;
        bun) run=(bun) ;;
        deno) run=(deno run -A) ;;
        *)
            echo "unknown runtime $runtime" >&2
            exit 2
            ;;
    esac
    # Plug'n'Play has no node_modules; `yarn node` adds Yarn's loader.
    if [ "$installer" = yarn ]; then
        run=(npx --yes -p @yarnpkg/cli-dist@4 yarn "${run[@]}")
    fi

    group "$installer / $runtime"
    (cd "$dir" && "${run[@]}" smoke.mjs)
    status=$?
    endgroup
    if [ $status -eq 0 ]; then record ok "$installer / $runtime"; else record FAIL "$installer / $runtime"; fi
done

printf '%s\n' "${results[@]}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "### Package: ${RUNNER_OS:-} ${RUNNER_ARCH:-}"
        printf -- '- %s\n' "${results[@]}"
    } >>"$GITHUB_STEP_SUMMARY"
fi
[ $failed -eq 0 ] || { echo "$failed failed" >&2; exit 1; }
