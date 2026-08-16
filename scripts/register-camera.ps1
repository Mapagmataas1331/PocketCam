# Requires elevation. Copies the camera DLL to ProgramData and registers
# CLSID {7B89B92E-FE71-42D0-8A41-E137D06EA184} in HKLM for Frame Server.
# Close OBS / Windows Camera / Discord first so the DLL is not locked.
#Requires -RunAsAdministrator
param(
    [string]$DllPath
)

$ErrorActionPreference = "Stop"
$clsid = "{7B89B92E-FE71-42D0-8A41-E137D06EA184}"
$destDir = "C:\ProgramData\PocketCam"
$destDll = Join-Path $destDir "VirtualCameraMediaSource.dll"

if (-not $DllPath) {
    $DllPath = Join-Path $PSScriptRoot "..\redist\VirtualCameraMediaSource.dll"
}
$DllPath = [IO.Path]::GetFullPath($DllPath)
if (-not (Test-Path $DllPath)) {
    throw "DLL not found: $DllPath"
}

New-Item -ItemType Directory -Force -Path $destDir | Out-Null
Copy-Item -Force $DllPath $destDll

# SYSTEM + Local Service (Frame Server) + Users: read/execute only.
icacls $destDir /grant "*S-1-5-18:(OI)(CI)F" /grant "*S-1-5-19:(OI)(CI)RX" /grant "Users:(OI)(CI)RX" /T | Out-Null

$inproc = "HKLM:\SOFTWARE\Classes\CLSID\$clsid\InProcServer32"
New-Item -Path "HKLM:\SOFTWARE\Classes\CLSID\$clsid" -Force | Out-Null
New-Item -Path $inproc -Force | Out-Null
Set-ItemProperty -Path $inproc -Name "(default)" -Value $destDll
Set-ItemProperty -Path $inproc -Name "ThreadingModel" -Value "Both"

Write-Host "Registered $clsid -> $destDll"
Get-ItemProperty $inproc | Format-List
