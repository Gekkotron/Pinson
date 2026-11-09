#!/usr/bin/env bash
# Rebuilds web/vendor/sendspin-js-6.0.0.js: @sendspin/sendspin-js bundled as one minified ES module.
#
# Pinned to 6.0.0 on purpose. 5.0.0 reads a 9-byte audio chunk header, but nidoo-sound sends a
# 13-byte one, which plays back as audible crackling; 6.0.0 reads the 13-byte header.
# Do not bump the version without checking against a real nidoo-sound.
set -euo pipefail

SENDSPIN_VERSION=6.0.0
ESBUILD_VERSION=0.24.2

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cd "$work"
npm init -y >/dev/null
npm install --no-audit --no-fund --save-exact \
  "@sendspin/sendspin-js@${SENDSPIN_VERSION}" "esbuild@${ESBUILD_VERSION}"
echo "export { SendspinPlayer } from '@sendspin/sendspin-js';" > entry.js
npx esbuild entry.js --bundle --format=esm --target=es2020 --minify \
  --outfile="$here/sendspin-js-${SENDSPIN_VERSION}.js"
cp node_modules/@sendspin/sendspin-js/LICENSE "$here/LICENSE-sendspin-js.txt"
