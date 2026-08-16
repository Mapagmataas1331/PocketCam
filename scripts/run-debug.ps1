# Convenience wrapper; same as `.\run.ps1 -Debug`.
& (Join-Path $PSScriptRoot "run.ps1") -Debug @args
