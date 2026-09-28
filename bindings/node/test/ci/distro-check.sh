#!/usr/bin/env bash
# In a Docker container: installs the package with npm, runs the smoke test,
# and checks the outcome is <expect>: ok, alsa (the install-ALSA hint), or
# no-prebuild.
#
#   distro-check.sh <expect> <tarball, or its directory>
set -uo pipefail

expect=$1
tarball=$2
here=$(cd "$(dirname "$0")" && pwd)
. /etc/os-release
echo "== $PRETTY_NAME, node $(node --version), $(ldd --version 2>&1 | head -n1)"

"$here/install-package.sh" npm "$tarball" /tmp/smoke >/dev/null || exit 1
cd /tmp/smoke
out=$(node smoke.mjs 2>&1)
status=$?
echo "$out"

if [ $status -eq 0 ]; then
    echo "RESULT ($PRETTY_NAME): works"
elif grep -q "Install the ALSA library" <<<"$out"; then
    echo "RESULT ($PRETTY_NAME): does not load without libasound2 (clean error)"
else
    echo "RESULT ($PRETTY_NAME): does not load (exit $status)"
fi

case $expect in
    ok) [ $status -eq 0 ] ;;
    alsa) [ $status -ne 0 ] && grep -q "Install the ALSA library" <<<"$out" ;;
    no-prebuild) [ $status -ne 0 ] && grep -q "has no prebuilt binary for linux-.*-musl" <<<"$out" ;;
    *) echo "unknown expectation $expect" >&2; false ;;
esac || { echo "expected: $expect" >&2; exit 1; }
echo "as expected: $expect"
