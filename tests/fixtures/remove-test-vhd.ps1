# Detaches and deletes the VHDX created by new-test-vhd.ps1. Requires an elevated shell.
param([string]$Path = (Join-Path $env:TEMP 'ferrofs-test.vhdx'))
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Path)) { return }
$scriptFile = New-TemporaryFile
Set-Content $scriptFile "select vdisk file=`"$Path`"`ndetach vdisk" -Encoding ascii
diskpart /s $scriptFile | Out-Null
Remove-Item $scriptFile
Remove-Item $Path
