#requires -Version 5.1
<#
.SYNOPSIS
    Build the SMS microservice (and optionally the bundled web UI).

.DESCRIPTION
    Stub build script. When fleshed out this will:
      1. Optionally build the web UI under `web-ui/` with Bun + Vite
         (`bun install` then `bun run build`) to produce static assets.
      2. Build the Rust service with `cargo build --release`, enabling the
         `web-ui` feature when the front-end was built so the assets are bundled.

.PARAMETER WebUi
    Build and bundle the web UI (enables the `web-ui` cargo feature).

.PARAMETER Release
    Build in release mode.
#>
[CmdletBinding()]
param(
    [switch]$WebUi,
    [switch]$Release
)

$ErrorActionPreference = "Stop"

Write-Host "[build] stub - not yet implemented" -ForegroundColor Yellow
Write-Host "[build]   WebUi   = $WebUi"
Write-Host "[build]   Release = $Release"

# TODO: build web-ui with Bun (bun install; bun run build) when -WebUi is set.
# TODO: cargo build in service/ (add --release and --features web-ui as appropriate).

exit 0
