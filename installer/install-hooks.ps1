# Called elevated by Inno Setup. Do not run by hand unless debugging Setup.
#Requires -RunAsAdministrator
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("install", "uninstall")]
    [string]$Mode,

    [string]$AppDir
)

$ErrorActionPreference = "Stop"
$rule = "PocketCam HTTPS"
$dataDir = Join-Path $env:ProgramData "PocketCam"

if ($Mode -eq "install") {
    if (-not $AppDir) {
        throw "AppDir is required for install"
    }
    $exe = Join-Path $AppDir "pocketcam.exe"
    if (-not (Test-Path $exe)) {
        throw "pocketcam.exe not found: $exe"
    }

    New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
    icacls $dataDir /grant "*S-1-5-18:(OI)(CI)F" /grant "*S-1-5-19:(OI)(CI)RX" /grant "Users:(OI)(CI)RX" /T | Out-Null

    # Stopgap register-camera.ps1 left the DLL here. The installer DLL lives in Program Files.
    $oldDll = Join-Path $dataDir "VirtualCameraMediaSource.dll"
    if (Test-Path $oldDll) {
        Remove-Item -Force $oldDll
    }

    cmd /c "netsh advfirewall firewall delete rule name=`"$rule`" >nul 2>&1"
    netsh advfirewall firewall add rule name="$rule" dir=in action=allow program="$exe" enable=yes profile=private protocol=TCP
    if ($LASTEXITCODE -ne 0) {
        throw "failed to add firewall rule $rule"
    }
    exit 0
}

cmd /c "netsh advfirewall firewall delete rule name=`"$rule`" >nul 2>&1"

# Frame Server (Local Service) keeps VirtualCameraMediaSource.dll mapped.
# Stop it so uninstall can delete the DLL. It starts again on the next camera use.
Get-Process -Name pocketcam -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
foreach ($name in @("FrameServer", "FrameServerMonitor")) {
    $svc = Get-Service -Name $name -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -eq "Running") {
        Stop-Service -Name $name -Force -ErrorAction SilentlyContinue
    }
}
Start-Sleep -Milliseconds 800

if ($AppDir) {
    $dll = Join-Path $AppDir "VirtualCameraMediaSource.dll"
    if (Test-Path $dll) {
        Remove-Item -LiteralPath $dll -Force -ErrorAction SilentlyContinue
    }
}

$ring = Join-Path $dataDir "nv12.ring"
if (Test-Path $ring) {
    Remove-Item -LiteralPath $ring -Force -ErrorAction SilentlyContinue
}
exit 0
