# End-to-end test of the administrator flows of ferro-storage. Needs the user to approve
# three UAC prompts. Everything happens in a throwaway fixture under %TEMP%:
#   fixture\tmp, fixture\windows\Temp   "your temp" and "Windows temp" (WINDIR points here)
#   fixture\locked\App                   readable by Administrators and SYSTEM only
# Steps (this script runs unelevated):
#   1. cleanup unelevated: Windows temp shows "needs admin"; click Restart as administrator
#      (UAC 1) -> an elevated instance must start with --cleanup <folder>
#   2. compression unelevated on locked\App: unreadable; click Restart as administrator
#      (UAC 2) -> an elevated instance must start with --compression <folder>
#   3. scripts\e2e-admin-elevated.ps1 (UAC 3) checks both instances (elevated token, command
#      line, screenshots), closes them, then drives its own elevated instances with the
#      fixture environment: cleanup lists Windows temp as cleanable; compression compresses
#      and decompresses locked\App. Elevated windows can only be clicked from an elevated
#      process (UIPI), hence the elevated helper.
#   cargo build -p ferro-service --examples; cargo build -p ferro-storage
#   .\scripts\e2e-admin.ps1 -Repo $PWD -Out target\admin
param([string]$Repo, [string]$Out, [switch]$ShotOnly)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'e2e-ui-helpers.ps1')
$script:failures = 0

$sandbox = Join-Path $env:TEMP 'ferrofs-e2e-admin'
$fx = "$sandbox\fixture"
if (Test-Path $sandbox) { icacls $sandbox /reset /T /Q | Out-Null; Remove-Item -Recurse -Force $sandbox }
New-Item -ItemType Directory -Force $Out, "$fx\tmp", "$fx\windows\Temp", "$fx\local", "$fx\profile", "$fx\dev", "$fx\locked\App\bin" | Out-Null
Fill "$fx\tmp\setup.tmp" 1
Fill "$fx\windows\Temp\old-update.tmp" 2
[IO.File]::WriteAllBytes("$fx\locked\App\bin\core.dll", (Text 8 'core'))
[IO.File]::WriteAllBytes("$fx\locked\App\data.xml", (Text 4 'data'))
# Administrators and SYSTEM only; the unelevated token has Administrators deny-only.
icacls "$fx\locked" /inheritance:r /grant:r '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-18:(OI)(CI)F' /Q | Out-Null
Check 'locked folder is unreadable here' (-not (Test-Path "$fx\locked\App\bin\core.dll" -ErrorAction SilentlyContinue))

$fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList "`"$fx`"" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$exe = Join-Path $Repo 'target\debug\ferro-storage.exe'
try {
  # 1. Cleanup.
  $ui = Start-Fixture-Ui $exe @('--cleanup', "`"$fx\dev`"") $fx
  $h = Wait-Window $ui.Id
  Shot $h 'a1-cleanup-unelevated'
  if (-not $ShotOnly) {
    "approve the UAC prompt for the cleanup restart"
    Click-At $h 1261 92                     # Restart as administrator
    $elevatedCleanup = Wait-New-Process $ui.Id 180
    Check 'cleanup restarted in a new process' ($null -ne $elevatedCleanup) $elevatedCleanup
    Check 'unelevated cleanup closed itself' ($null -eq (Get-Process -Id $ui.Id -ErrorAction SilentlyContinue))

    # 2. Compression on the locked folder.
    $ui = Start-Fixture-Ui $exe @('--compression', "`"$fx\locked\App`"") $fx
    $h = Wait-Window $ui.Id
    Shot $h 'a2-compression-unelevated'
    "approve the UAC prompt for the compression restart"
    Click-At $h 1075 92                     # Restart as administrator
    $known = @($ui.Id) + @($elevatedCleanup | Where-Object { $_ })
    $elevatedCompression = Wait-New-Process $known 180
    Check 'compression restarted in a new process' ($null -ne $elevatedCompression) $elevatedCompression

    # 3. The elevated part.
    "approve the UAC prompt for the elevated test helper"
    $helper = Join-Path $PSScriptRoot 'e2e-admin-elevated.ps1'
    Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$helper`"", '-Repo', "`"$Repo`"", '-Out', "`"$Out`"", '-Fixture', "`"$fx`""
    $report = Join-Path $Out 'elevated.txt'
    if (Test-Path $report) {
      Get-Content $report
      $script:failures += @(Get-Content $report | Where-Object { $_ -match '\sFAIL\s' }).Count
    } else {
      Check 'elevated helper ran' $false 'no report'
    }
  }
} finally {
  Get-Process ferro-storage -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $null } | ForEach-Object {
    try { Stop-Process -Id $_.Id -Force -ErrorAction Stop } catch { "cannot stop $($_.Id) (elevated?)" }
  }
  Stop-Process -Id $fake.Id -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
  # The elevated helper restored the permissions; fall back to resetting them here (as owner).
  icacls "$fx\locked" /reset /Q 2>$null | Out-Null
  icacls $sandbox /reset /T /Q 2>$null | Out-Null
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
  Check 'fixture removed' (-not (Test-Path $sandbox))
}
"failures: $failures"
