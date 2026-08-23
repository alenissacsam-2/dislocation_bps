# Build cryptobot for Windows.
#
# The one thing this exists to prevent: a bare `cargo build` writes to .\target, which
# is *not* where anything looks for the binary. The build succeeds, and whatever was
# there before keeps running - silently. That failure has already cost this project two
# builds' worth of confusion under WSL, where scripts/env.sh was written to stop it.
# This is the same guard for the same reason.
#
#   scripts\build.ps1            # release build of the bot and the app
#   scripts\build.ps1 -Debug     # faster, for iterating on the UI
#
# Requires Visual Studio Build Tools (the C++ workload). Without a linker, cargo fails
# late and confusingly; this checks up front and says so.

# Not -Debug: that is one of PowerShell's own common parameters and collides.
param([switch]$Dev)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Keep the target directory off the repo, and identical to every other entry point.
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\cryptobot-win-target"

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    Write-Error @'
No Visual Studio Build Tools found, so Rust has no linker and this build cannot finish.
Install once with:

  winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
'@
}

$profileArg = if ($Dev) { @() } else { @('--release') }
$profileDir = if ($Dev) { 'debug' } else { 'release' }

Write-Host "building -> $env:CARGO_TARGET_DIR\$profileDir"
cargo build @profileArg -p cb-bot -p cb-desk
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

foreach ($exe in 'cb-bot.exe', 'cryptobot-desk.exe') {
    $p = Join-Path "$env:CARGO_TARGET_DIR\$profileDir" $exe
    if (Test-Path $p) {
        $mb = (Get-Item $p).Length / 1MB
        '{0,-20} {1,6:N1} MB' -f $exe, $mb
    } else {
        Write-Warning "$exe missing - the build reported success but produced nothing"
    }
}

Write-Host ''
Write-Host 'Run it with the application, from the repository root:'
Write-Host "  $env:CARGO_TARGET_DIR\$profileDir\cryptobot-desk.exe --start"
