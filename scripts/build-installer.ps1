# Builds the release binaries and the Inno Setup installer:
#   .\scripts\build-installer.ps1            -> target\installer\ferrofs-setup-<version>.exe
# Needs Inno Setup 6 (winget install JRSoftware.InnoSetup). The version comes from the
# workspace Cargo.toml.
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

$iscc = @(
  (Get-Command ISCC.exe -ErrorAction SilentlyContinue | ForEach-Object Source),
  "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
  "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
  "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup)' }

$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq 'ferro-service').version
if (-not $SkipBuild) {
  cargo build --release -p ferro-service -p ferro-search -p ferro-storage -p ferro-link
  if ($LASTEXITCODE) { throw 'cargo build failed' }
}
foreach ($bin in 'ferro-service', 'ferro-search', 'ferro-storage', 'ferro-link') {
  if (-not (Test-Path "target\release\$bin.exe")) { throw "missing target\release\$bin.exe" }
}
& $iscc /Q "/DAppVersion=$version" "/DBinDir=$repo\target\release" "/DOutputDir=$repo\target\installer" "$repo\installer\ferrofs.iss"
if ($LASTEXITCODE) { throw "ISCC failed ($LASTEXITCODE)" }
$setup = "$repo\target\installer\ferrofs-setup-$version.exe"
# Keep a copy outside target\, so `cargo clean` does not delete the last installer.
New-Item -ItemType Directory -Force "$repo\dist" | Out-Null
Copy-Item $setup "$repo\dist\" -Force
"{0} ({1:N1} MB)" -f $setup, ((Get-Item $setup).Length / 1MB)
