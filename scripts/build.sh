#!/usr/bin/env bash
# Build the SMS microservice (and optionally the bundled web UI).
#
# Usage:
#   scripts/build.sh [--release] [--web-ui] [--target <triple>]
#
# Builds the Rust service in service/. With --web-ui it first builds the
# Next.js front-end in web-ui/ with Bun and enables the `web-ui` cargo feature.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
service_dir="$repo_root/service"
web_ui_dir="$repo_root/web-ui"

release=0
web_ui=0
target=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) release=1; shift ;;
        --web-ui) web_ui=1; shift ;;
        --target) target="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ "$web_ui" -eq 1 ]]; then
    echo "[build] building web UI with Bun"
    command -v bun >/dev/null 2>&1 || { echo "bun is not installed; see https://bun.sh" >&2; exit 1; }
    ( cd "$web_ui_dir" && bun install && bun run build )
fi

args=(build)
[[ "$release" -eq 1 ]] && args+=(--release)
[[ -n "$target" ]] && args+=(--target "$target")
[[ "$web_ui" -eq 1 ]] && args+=(--features web-ui)

echo "[build] cargo ${args[*]}"
( cd "$service_dir" && cargo "${args[@]}" )
echo "[build] done"
