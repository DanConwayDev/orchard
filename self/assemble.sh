#!/usr/bin/env bash
# Assemble Orchard's own app tree for self-distribution: the built
# binary into files/bin, beside the committed metadata and export
# files that orchard-publish turns into a flatpak-shaped OCI image.
# The freedesktop runtime supplies the native libraries the binary
# links (wayland, xkbcommon, fontconfig), so nothing else ships.
set -euo pipefail
cd "$(dirname "$0")/.."

binary=target/release/orchard
if [[ ! -x $binary ]]; then
    echo "missing $binary; run ./scripts/build.sh build --release first" >&2
    exit 1
fi

mkdir -p self/tree/files/bin
install -m 0755 "$binary" self/tree/files/bin/orchard
echo "assembled self/tree: $(du -sh self/tree/files | cut -f1)"
