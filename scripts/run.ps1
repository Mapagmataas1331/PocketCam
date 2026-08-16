# Kill a leftover host, then cargo run. Default is --release.
param(
    [switch]$Debug
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path $cargoBin) {
    $env:PATH = "$cargoBin;$env:PATH"
}

Get-Process -Name pocketcam -ErrorAction SilentlyContinue | Stop-Process -Force

# Env var, not a cargo/exe argument. Abort-on-OOM still has no Rust backtrace.
$env:RUST_BACKTRACE = "1"

if ($Debug) {
    Write-Host "Building and running PocketCam (debug)..."
    cargo run
} else {
    Write-Host "Building and running PocketCam (release)..."
    cargo run --release
}
