# Full lifecycle test of the installer. Run elevated (it installs for real):
#   Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"$PWD\scripts\test-installer.ps1"
# Silent install with both autostart tasks -> checks (files, service running from the
# install folder, index ready, autostart values, shortcuts, Programs entry) -> install
# again over it (upgrade path) -> silent uninstall -> everything gone. With -KeepInstalled
# the last install stays. Report: target\installer-test.log
param([switch]$KeepInstalled)
$ErrorActionPreference = 'Continue'
$repo = Split-Path $PSScriptRoot -Parent
$log = Join-Path $repo 'target\installer-test.log'
$setup = Get-ChildItem "$repo\target\installer\ferrofs-setup-*.exe" | Sort-Object LastWriteTime | Select-Object -Last 1
$app = Join-Path $env:ProgramFiles 'ferrofs'
$data = Join-Path $env:ProgramData 'ferrofs'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$uninstallKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{7E0C2B1A-5F3D-4C8E-9A61-2D4B8F0E6C13}_is1'
$script:failures = 0
"=== $(Get-Date -Format s) $($setup.Name) ===" | Set-Content $log
function Log($text) { $text | Add-Content $log }
function Check($name, $ok, $detail) {
  if (-not $ok) { $script:failures++ }
  Log ("{0,-44} {1} {2}" -f $name, ($(if ($ok) { 'OK  ' } else { 'FAIL' })), $detail)
}
function Install($name) {
  $p = Start-Process $setup.FullName -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS="autostartsearch,autostartlink"', "/LOG=`"$repo\target\installer-$name.log`"" -Wait -PassThru
  Check "$name exits with 0" ($p.ExitCode -eq 0) $p.ExitCode
}
function Wait-Ready {
  $deadline = (Get-Date).AddSeconds(90)
  do {
    Start-Sleep -Seconds 2
    $status = & "$app\ferro-service.exe" status 2>&1 | Out-String
  } until (($status -match 'Ready' -and $status -notmatch 'Scanning') -or (Get-Date) -gt $deadline)
  $status
}
function Check-Installed($name) {
  foreach ($bin in 'ferro-service', 'ferro-search', 'ferro-storage', 'ferro-link', 'unins000') {
    Check "$name`: $bin.exe installed" (Test-Path "$app\$bin.exe")
  }
  $svc = Get-CimInstance Win32_Service -Filter "Name = 'ferrofs'"
  Check "$name`: service running" ($svc.State -eq 'Running') $svc.State
  Check "$name`: service starts automatically" ($svc.StartMode -eq 'Auto') $svc.StartMode
  Check "$name`: service runs the installed exe" ($svc.PathName -match [regex]::Escape("$app\ferro-service.exe")) $svc.PathName
  Check "$name`: service runs as LocalSystem" ($svc.StartName -eq 'LocalSystem') $svc.StartName
  $status = Wait-Ready
  Check "$name`: index ready" ($status -match 'C:\\\s+Ready') ($status -replace '\s+', ' ')
  $query = & "$app\ferro-service.exe" query 'ferro-service.exe' 2>&1 | Out-String
  Check "$name`: search finds the installed exe" ($query -match [regex]::Escape("$app\ferro-service.exe")) (($query -split "`n" | Select-Object -Last 2) -join ' ')
  $run = Get-ItemProperty $runKey
  Check "$name`: search starts at sign-in" ($run.'ferrofs-search' -eq "`"$app\ferro-search.exe`"") $run.'ferrofs-search'
  Check "$name`: link starts at sign-in" ($run.'ferrofs-link' -eq "`"$app\ferro-link.exe`"") $run.'ferrofs-link'
  $menu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'
  Check "$name`: Start menu shortcuts" ((Test-Path "$menu\ferrofs search.lnk") -and (Test-Path "$menu\ferrofs storage.lnk") -and (Test-Path "$menu\ferrofs link.lnk"))
  $entry = Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue
  Check "$name`: listed in installed apps" ($entry.DisplayName -like 'ferrofs *') $entry.DisplayName
}

try {
  Check 'installer found' ($null -ne $setup) $setup.FullName
  Check 'running elevated' (([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))
  Check 'no ferrofs service before' ($null -eq (Get-Service ferrofs -ErrorAction SilentlyContinue))

  Install 'install'
  Check-Installed 'install'
  Check 'snapshot folder created' (Test-Path "$data\index")

  Install 'reinstall'
  Check-Installed 'reinstall'

  if (-not $KeepInstalled) {
    $p = Start-Process "$app\unins000.exe" -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$repo\target\installer-uninstall.log`"" -Wait -PassThru
    Check 'uninstall exits with 0' ($p.ExitCode -eq 0) $p.ExitCode
    # The uninstaller hands off to a temporary copy; wait for the folder to go.
    $deadline = (Get-Date).AddSeconds(30)
    while ((Test-Path $app) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
    Check 'service removed' ($null -eq (Get-Service ferrofs -ErrorAction SilentlyContinue))
    Check 'install folder removed' (-not (Test-Path $app))
    $run = Get-ItemProperty $runKey
    Check 'autostart values removed' ($null -eq $run.'ferrofs-search' -and $null -eq $run.'ferrofs-link')
    Check 'shortcuts removed' (-not (Test-Path (Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\ferrofs search.lnk')))
    Check 'snapshots and service logs removed' (-not (Test-Path $data))
    Check 'not listed in installed apps' ($null -eq (Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue))
  }
} catch {
  Log "ERROR: $_"
  $script:failures++
}
Log "failures: $failures"
