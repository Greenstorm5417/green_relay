#!/usr/bin/env bash
# Emit the generated OpenAPI document to a file (default: openapi.json).
#
# Usage: scripts/gen-openapi.sh [output-path]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-$repo_root/openapi.json}"

( cd "$repo_root/service" && cargo run --quiet --bin green_relay -- openapi ) > "$out"
echo "[gen-openapi] wrote $out"
