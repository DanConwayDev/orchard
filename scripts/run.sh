#!/usr/bin/env bash
# Runs the built client on the host. The host Wayland session is required,
# and lib/ bundles libxkbcommon-x11 (the atomic host image lacks it at
# runtime; the dev container only needs it to link).
set -euo pipefail
cd "$(dirname "$0")/.."

export LD_LIBRARY_PATH="$PWD/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec ./target/release/orchard "$@"
