#!/usr/bin/env bash
# Start the Windows build with UMU and its Steam Linux Runtime.
set -eu

usage() {
    echo 'Usage: victory-proton.sh TITLE.exe [--execute] [other victory run options]'
}

if [[ ${1:-} == --help ]]; then
    usage
    exit 0
fi
if (( $# == 0 )); then
    usage >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
target_dir=${CARGO_TARGET_DIR:-$script_dir/../target}
binary=${VICTORY_EXE:-}
if [[ -z $binary ]]; then
    for candidate in "$script_dir/victory.exe" \
        "$target_dir/x86_64-pc-windows-gnullvm/release/victory.exe" \
        "$target_dir/x86_64-pc-windows-msvc/release/victory.exe"; do
        if [[ -f $candidate ]]; then
            binary=$candidate
            break
        fi
    done
fi
if [[ ! -f $binary ]]; then
    echo "Missing Windows build: ${binary:-set VICTORY_EXE or see docs/proton-launcher.md}" >&2
    exit 1
fi
if [[ ! -f $1 ]]; then
    echo "Missing title executable: $1" >&2
    exit 1
fi
if ! command -v umu-run >/dev/null; then
    echo 'Install umu-launcher to provide umu-run.' >&2
    exit 1
fi
binary=$(realpath -- "$binary")
title=$(realpath -- "$1")
shift

# Hand Proton Windows paths on its standard Z: drive. A Windows executable
# path also lets UMU-Proton attach this CLI to the calling terminal.
export GAMEID=${GAMEID:-umu-victory}
export WINEPREFIX=${WINEPREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton}
windows_title=${title//\//\\}
windows_binary=${binary//\//\\}
export STEAM_COMPAT_INSTALL_PATH=${STEAM_COMPAT_INSTALL_PATH:-$(dirname -- "$binary")}
exec umu-run "Z:$windows_binary" run "Z:$windows_title" "$@"
