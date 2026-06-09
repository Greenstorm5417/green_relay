#requires -Version 5.1
<#
.SYNOPSIS
    Install the SMS microservice and its build prerequisites.

.DESCRIPTION
    Stub install script. When fleshed out this will:
      1. Verify / install prerequisites (Rust toolchain, Bun).
      2. Install web UI dependencies under `web-ui/` (`bun install`).
      3. Install the built service binary to the target location.

.PARAMETER Prefix
    Installation prefix for the service binary.
#>
[CmdletBinding()]
param(
    [string]$Prefix
)

$ErrorActionPreference = "Stop"

Write-Host "[install] stub - not yet implemented" -ForegroundColor Yellow
Write-Host "[install]   Prefix = $Prefix"

# TODO: check for rustup/cargo and bun; guide installation if missing.
# TODO: bun install in web-ui/.
# TODO: place the built binary (from service/) under $Prefix.

exit 0
