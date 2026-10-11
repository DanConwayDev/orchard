#!/usr/bin/env bash
# Runs the built client on the host. The host Wayland session is required,
# and lib/ bundles libxkbcommon-x11 (the atomic host image lacks it at
# runtime; the dev container only needs it to link).
set -euo pipefail
cd "$(dirname "$0")/.."

export LD_LIBRARY_PATH="$PWD/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# The dev loop reads and writes the demo relay on this machine. Unset
# in your environment to exercise the shipped default (the public
# rendezvous set) instead.
export ORCHARD_RELAYS="${ORCHARD_RELAYS:-ws://127.0.0.1:8008}"
exec ./target/release/orchard "$@"
