#!/usr/bin/env bash
# Installs the package with each package manager (scripts off) and runs the
# smoke test under each runtime: node, bun, and deno, or `yarn node` for
# Yarn Plug'n'Play. Runs every combination, then fails if any failed.
#
#   package-matrix.sh <tarball, or its directory> <work dir> [installer...]
#
# HANDY_RECORDER_SMOKE_DEVICE is passed through to the smoke test.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
package=$1
work=$2
shift 2
installers=("$@")
[ ${#installers[@]} -gt 0 ] || installers=(npm pnpm bun yarn)
mkdir -p "$work"

group() { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::group::$1" || echo "== $1"; }
endgroup() { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::endgroup::" || true; }

results=()
failed=0
record() {
    results+=("$1 $2")
    [ "$1" = ok ] || failed=$((failed + 1))
}

for installer in "${installers[@]}"; do
    dir="$work/$installer"
    group "install with $installer"
    "$here/install-package.sh" "$installer" "$package" "$dir"
    status=$?
    endgroup
    if [ $status -ne 0 ]; then
        record FAIL "$installer: install"
        continue
    fi
    if [ "$installer" = yarn ]; then
        runtimes=("yarn node")
    else
        runtimes=(node bun "deno run -A")
    fi
    for runtime in "${runtimes[@]}"; do
        group "$installer / $runtime"
        if [ "$runtime" = "yarn node" ]; then
            (cd "$dir" && npx --yes -p @yarnpkg/cli-dist@4 yarn node smoke.mjs)
        else
            # Word splitting intended: "deno run -A".
            # shellcheck disable=SC2086
            (cd "$dir" && $runtime smoke.mjs)
        fi
        status=$?
        endgroup
        if [ $status -eq 0 ]; then record ok "$installer / $runtime"; else record FAIL "$installer / $runtime"; fi
    done
done

printf '%s\n' "${results[@]}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "### Package: ${RUNNER_OS:-} ${RUNNER_ARCH:-}${HANDY_RECORDER_SMOKE_DEVICE:+, recording from $HANDY_RECORDER_SMOKE_DEVICE}"
        printf -- '- %s\n' "${results[@]}"
    } >>"$GITHUB_STEP_SUMMARY"
fi
[ $failed -eq 0 ] || { echo "$failed failed" >&2; exit 1; }
