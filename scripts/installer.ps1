# Build the Windows installer for cryptobot.
#
# What this produces is a single .exe that installs the application and the bot next to
# each other, per-user, with no admin prompt. It is the only supported way to hand this
# to a machine that does not have the repository on it.
#
#   scripts\installer.ps1
#
# Two things have to be true for the result to actually work, and both are done here
# rather than left to whoever runs the build:
#
#   1. cb-bot.exe ships beside the app. `Paths::bot_exe` looks next to the executable
#      first, which is what a shipped install looks like. Tauri calls this a sidecar and
#      insists on the target triple in the filename; nothing else about the name matters.
#      The declaration lives in tauri.installer.conf.json and is merged in only here,
#      because declaring it in tauri.conf.json makes a staged sidecar a precondition of
#      every `cargo build` and `cargo test` in the workspace, CI included.
#   2. The app has a writable place to record. It does not: Program Files is not
#      user-writable, so the installed app puts its config and ledger under
#      %LOCALAPPDATA%\cryptobot instead. That is `Paths::data_dir`, seeded on first run.
#
# Requires Visual Studio Build Tools and the Tauri CLI:
#
#   cargo install tauri-cli --version "^2" --locked

param([switch]$KeepStaged)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Identical to build.ps1, and for the same reason: a bare cargo build writes to .\target,
# which is not where anything looks.
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\cryptobot-win-target"

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    Write-Error @'
No Visual Studio Build Tools found, so Rust has no linker and this build cannot finish.
Install once with:

  winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
'@
}

if (-not (Get-Command cargo-tauri -ErrorAction SilentlyContinue)) {
    Write-Error @'
The Tauri CLI is not installed, and it is what builds the bundle. Install it with:

  cargo install tauri-cli --version "^2" --locked
'@
}

# The bundler will not start if a previous copy is holding the binaries open.
foreach ($name in 'cryptobot-desk', 'cb-bot') {
    Get-Process -Name $name -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "stopping running $name (pid $($_.Id)) so the build can replace it"
        Stop-Process -Id $_.Id -Force
    }
}

Write-Host 'building the bot...'
cargo build --release -p cb-bot
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Tauri resolves a sidecar as <name>-<target-triple><ext>, so ask rustc what the triple
# is rather than hardcoding one and being wrong on the first non-x64 machine.
$triple = (rustc -vV | Select-String '^host: ').Line -replace '^host: ', ''
$staged = Join-Path $root 'crates\desk\binaries'
New-Item -ItemType Directory -Force -Path $staged | Out-Null

$botSrc = Join-Path "$env:CARGO_TARGET_DIR\release" 'cb-bot.exe'
if (-not (Test-Path $botSrc)) {
    Write-Error "cb-bot.exe is missing at $botSrc - the build reported success but produced nothing"
}
$botDst = Join-Path $staged "cb-bot-$triple.exe"
Copy-Item $botSrc $botDst -Force
Write-Host "staged sidecar -> $botDst"

Write-Host 'bundling the installer...'
Push-Location (Join-Path $root 'crates\desk')
try {
    cargo tauri build --config tauri.installer.conf.json
    $code = $LASTEXITCODE
} finally {
    Pop-Location
    # Staged copies are build output, not source. Left behind they go stale silently and
    # the next installer ships whichever bot happened to be there.
    if (-not $KeepStaged) { Remove-Item $staged -Recurse -Force -ErrorAction SilentlyContinue }
}
if ($code -ne 0) { exit $code }

$out = Join-Path "$env:CARGO_TARGET_DIR\release\bundle" 'nsis'
$installers = @(Get-ChildItem -Path $out -Filter '*.exe' -ErrorAction SilentlyContinue)
if ($installers.Count -eq 0) {
    Write-Error "the bundler finished but produced no installer in $out"
}

Write-Host ''
Write-Host 'Installer built:'
foreach ($i in $installers) {
    '{0,-46} {1,6:N1} MB' -f $i.Name, ($i.Length / 1MB)
    Write-Host "  $($i.FullName)"
}
Write-Host ''
Write-Host 'It installs per-user (no admin prompt), and on first launch records to:'
Write-Host "  $env:LOCALAPPDATA\cryptobot"
