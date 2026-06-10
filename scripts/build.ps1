#requires -Version 5.1
<#
.SYNOPSIS
    Build the SMS microservice (and optionally the bundled web UI).

.DESCRIPTION
    Builds the Rust service in `service/`. When -WebUi is set, first builds the
    Next.js front-end in `web-ui/` with Bun (static export to `web-ui/out`) and
    enables the `web-ui` cargo feature so the assets are served by the service.

.PARAMETER WebUi
    Build and bundle the web UI (enables the `web-ui` cargo feature).

.PARAMETER Release
    Build in release mode.

.EXAMPLE
    ./scripts/build.ps1 -Release
.EXAMPLE
    ./scripts/build.ps1 -WebUi -Release
#>
[CmdletBinding()]
param(
    [switch]$WebUi,
    [switch]$Release
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$serviceDir = Join-Path $repoRoot "service"
$webUiDir = Join-Path $repoRoot "web-ui"

if ($WebUi) {
    Write-Host "[build] building web UI with Bun" -ForegroundColor Cyan
    if (-not (Get-Command bun -ErrorAction SilentlyContinue)) {
        throw "bun is not installed; install it from https://bun.sh or omit -WebUi"
    }
    bun install --cwd $webUiDir
    bun run --cwd $webUiDir build
}

$cargoArgs = @("build")
if ($Release) { $cargoArgs += "--release" }
if ($WebUi) { $cargoArgs += @("--features", "web-ui") }

Write-Host "[build] cargo $($cargoArgs -join ' ')" -ForegroundColor Cyan
Push-Location $serviceDir
try {
    cargo @cargoArgs
}
finally {
    Pop-Location
}

Write-Host "[build] done" -ForegroundColor Green
