#!/bin/sh
# Run the H4 aliasing check under Miri with several configurations.
# Usage: run_miri.sh <miri-driver> <miri-sysroot>
set -u
MIRI="$1"
SYSROOT="$2"
cd "$(dirname "$0")"
for flags in "" "-Zmiri-tree-borrows" "-Zmiri-seed=3" "-Zmiri-seed=5 -Zmiri-tree-borrows" "-Zmiri-preemption-rate=0.9" "-Zmiri-many-seeds=0..16"; do
  echo "== miri $flags"
  # shellcheck disable=SC2086
  "$MIRI" --sysroot "$SYSROOT" -Zmiri-disable-isolation $flags miri_check.rs 2>&1 | tail -4
done
