#!/usr/bin/env bash
# Installs a packed @handy-computer/recorder into a new project, with
# install scripts off (as pi-voice's CI and many users have it), and copies
# in the smoke test.
#
#   install-package.sh npm|pnpm|bun|yarn <tarball, or its directory> <project dir>
#
# yarn is Yarn Berry with Plug'n'Play: run the smoke test with `yarn node`.
set -euo pipefail

installer=$1
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
