#!/usr/bin/env bash
# Installs the packed package into a new project, install scripts off, and
# copies in the smoke test. "yarn" is Yarn Plug'n'Play: run with `yarn node`.
#
#   install-package.sh npm|pnpm|bun|yarn <tarball, or its folder> <project dir>
set -euo pipefail

installer=$1
# Git Bash on Windows.
if command -v cygpath >/dev/null; then set -- "$1" "$(cygpath -u "$2")" "$3"; fi
if [ -d "$2" ]; then
    tarball=$(ls "$2"/*.tgz)
else
    tarball=$2
fi
tarball=$(cd "$(dirname "$tarball")" && pwd)/$(basename "$tarball")
dir=$3
here=$(cd "$(dirname "$0")" && pwd)
if command -v cygpath >/dev/null; then tarball=$(cygpath -m "$tarball"); fi

mkdir -p "$dir"
cd "$dir"
echo '{ "name": "smoke", "private": true, "type": "module" }' >package.json
cp "$here/../smoke.mjs" .

case $installer in
    npm) npm install --ignore-scripts --no-audit --no-fund "$tarball" ;;
    pnpm) npx --yes pnpm@10 add --ignore-scripts "$tarball" ;;
    bun) bun add --ignore-scripts "$tarball" ;;
    yarn)
        printf 'nodeLinker: pnp\nenableScripts: false\nenableGlobalCache: false\n' >.yarnrc.yml
        touch yarn.lock
        npx --yes -p @yarnpkg/cli-dist@4 yarn add "@handy-computer/recorder@file:$tarball"
        ;;
    *)
        echo "unknown installer $installer" >&2
        exit 2
        ;;
esac
