#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Rayl requires macOS and Apple Mail.' >&2
    exit 1
fi

release=https://github.com/oti-adjei/rayl/releases/latest/download
archive=rayl-macos-universal.tar.gz
bin_dir="$HOME/.local/bin"
work_dir=$(mktemp -d)
staged_binary=
cleanup() {
    rm -rf "$work_dir"
    if [ -n "$staged_binary" ]; then
        rm -f "$staged_binary"
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

cd "$work_dir"
curl -fsSL "$release/$archive" -o "$archive"
curl -fsSL "$release/$archive.sha256" -o "$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive" rayl

mkdir -p "$bin_dir"
staged_binary=$(mktemp "$bin_dir/.rayl.XXXXXXXX")
install -m 755 rayl "$staged_binary"
if [ -d "$bin_dir/rayl" ]; then
    printf 'Cannot install: %s is a directory.\n' "$bin_dir/rayl" >&2
    exit 1
fi
mv -f "$staged_binary" "$bin_dir/rayl"
printf 'Installed Rayl to %s\n' "$bin_dir/rayl"
printf 'Check Mail access with: "$HOME/.local/bin/rayl" doctor\n'
