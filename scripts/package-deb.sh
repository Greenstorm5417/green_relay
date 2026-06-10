#!/usr/bin/env bash
# Build a Debian package for the given target triple.
#
# Usage: scripts/package-deb.sh <target-triple> [--cross]
#
# Assumes the release binary has already been built for <target-triple>
# (e.g. by scripts/build.sh --release --target <triple> or `cross build`).
# Installs cargo-deb on demand. The release profile already strips the binary,
# so cargo-deb is invoked with --no-strip to avoid needing a target `strip`.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
service_dir="$repo_root/service"

target="${1:?usage: package-deb.sh <target-triple> [--cross]}"

command -v cargo-deb >/dev/null 2>&1 || cargo install cargo-deb --locked

echo "[package-deb] building .deb for $target"
( cd "$service_dir" && cargo deb --no-build --no-strip --target "$target" )

echo "[package-deb] artifacts:"
find "$service_dir/target/$target/debian" -name '*.deb' -print
