# Creates, attaches and formats a small NTFS VHDX for MFT/USN tests and prints its
# drive letter. Requires an elevated shell. Remove it with remove-test-vhd.ps1.
param(
  [string]$Path = (Join-Path $env:TEMP 'ferrofs-test.vhdx'),
  [int]$SizeMB = 256
)
$ErrorActionPreference = 'Stop'

$used = (Get-PSDrive -PSProvider FileSystem).Name
$letter = [char[]](70..90) | Where-Object { $used -notcontains [string]$_ } | Select-Object -Last 1
if (-not $letter) { throw 'no free drive letter' }
if (Test-Path $Path) { throw "$Path already exists; run remove-test-vhd.ps1 first" }

$script = @"
create vdisk file="$Path" maximum=$SizeMB type=expandable
select vdisk file="$Path"
attach vdisk
convert gpt
create partition primary
format fs=ntfs quick label=FERROTEST
assign letter=$letter
"@
$scriptFile = New-TemporaryFile
Set-Content $scriptFile $script -Encoding ascii
$output = diskpart /s $scriptFile
Remove-Item $scriptFile
if (-not (Test-Path "${letter}:\")) { $output | Write-Error; throw 'VHD was not mounted' }
"$letter"
