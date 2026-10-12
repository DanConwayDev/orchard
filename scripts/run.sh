#!/usr/bin/env bash
# Runs the built client on the host. The host Wayland session is required,
# and lib/ bundles libxkbcommon-x11 (the atomic host image lacks it at
# runtime; the dev container only needs it to link).
set -euo pipefail
cd "$(dirname "$0")/.."

export LD_LIBRARY_PATH="$PWD/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# The dev loop reads and writes the demo relay on this machine. Set it
# to empty (ORCHARD_RELAYS= ./scripts/run.sh) to exercise the shipped
# default (the public rendezvous set) instead: the client treats a
# blank value as no override.
export ORCHARD_RELAYS="${ORCHARD_RELAYS-ws://127.0.0.1:8008}"
# exec would lose it: the exit status is the only forensic trace of
# a silent death. 0 is a clean exit (an in-code path to hunt), 137
# SIGKILL, 143 SIGTERM (relaunch housekeeping), 139 a segfault.
status=0
./target/release/orchard "$@" || status=$?
echo "orchard exited: $status" >&2
exit "$status"
