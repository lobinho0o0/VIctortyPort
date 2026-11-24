#!/usr/bin/env bash
# Exercise the normal launcher and title entry point without any game files.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
title="$work/probe title.exe"
export WINEPREFIX=${VICTORY_SMOKE_PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton-smoke}
rustc --edition 2024 --target x86_64-pc-windows-gnullvm -C target-feature=+crt-static \
    -C panic=abort -C opt-level=2 \
    -C link-arg=-nostartfiles -C link-arg=-Wl,--entry=title_main \
    "$root/tests/fixtures/proton-smoke.rs" -o "$title" || {
    echo 'Building the smoke fixture requires the gnullvm Rust target and LLVM-MinGW (docs/proton-launcher.md).' >&2
    exit 1
}
"$root/scripts/victory-proton.sh" "$title" --execute 2>&1 | tee "$work/run.log"
# pipefail checks the launcher. Also check the return value printed by VIctory.
grep -Eq '^title exited with 42[[:space:]]*$' "$work/run.log"
