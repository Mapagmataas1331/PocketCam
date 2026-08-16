@echo off
setlocal
cd /d "%~dp0.."
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

taskkill /F /IM pocketcam.exe >nul 2>&1

rem Env var, not a cargo/exe argument. Abort-on-OOM still has no Rust backtrace.
set RUST_BACKTRACE=1

echo Building and running PocketCam (release)...
cargo run --release
endlocal
