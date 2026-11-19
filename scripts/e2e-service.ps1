# End-to-end test of ferro-service in console mode. Run elevated:
#   Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"$PWD\scripts\e2e-service.ps1"
# Mounts the test VHD, runs the service, checks search and live updates on the VHD, then
# writes target\e2e-service.ready and waits (up to 120 s) for target\e2e-service.done so an
# unelevated shell can query the pipe too. Log: target\e2e-service.log
$ErrorActionPreference = 'Continue'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo
$exe = Join-Path $repo 'target\release\ferro-service.exe'
$log = Join-Path $repo 'target\e2e-service.log'
$ready = Join-Path $repo 'target\e2e-service.ready'
$done = Join-Path $repo 'target\e2e-service.done'
Remove-Item $ready, $done -ErrorAction SilentlyContinue
"=== $(Get-Date -Format s) ===" | Set-Content $log
function Log($text) { $text | Add-Content $log }

$service = $null
try {
  $letter = & (Join-Path $repo 'tests\fixtures\new-test-vhd.ps1')
  $root = "${letter}:\"
  Log "test VHD at $root"
  New-Item -ItemType Directory "${root}e2e" | Out-Null
  Set-Content "${root}e2e\before-start.txt" 'x'

  $service = Start-Process $exe -ArgumentList 'run', '--console' -PassThru -WindowStyle Hidden `
    -RedirectStandardError (Join-Path $repo 'target\e2e-service.stderr.log')
  $deadline = (Get-Date).AddSeconds(90)
  do {
    Start-Sleep -Milliseconds 500
    $status = & $exe status 2>&1 | Out-String
  } until (($status -notmatch 'Scanning') -and ($status -match 'Ready') -or (Get-Date) -gt $deadline)
  Log "--- status ---`n$status"

  Log "--- query before-start (found by the scan) ---"
  & $exe query before-start *>> $log

  $unique = "live-$PID-$(Get-Random).txt"
  Set-Content "${root}e2e\$unique" 'x'
  Start-Sleep -Seconds 2
  Log "--- query $unique after create (journal) ---"
  & $exe query $unique *>> $log
  Remove-Item "${root}e2e\$unique"
  Start-Sleep -Seconds 2
  Log "--- query $unique after delete (journal) ---"
  & $exe query $unique *>> $log

  Log "--- bad query ---"
  & $exe query 'size:>1mb' *>> $log

  New-Item $ready | Out-Null
  $deadline = (Get-Date).AddSeconds(120)
  while (-not (Test-Path $done) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
  Log "done flag seen: $(Test-Path $done)"
} catch {
  Log "ERROR: $_"
} finally {
  if ($service) { Stop-Process -Id $service.Id -Force -ErrorAction SilentlyContinue }
  Start-Sleep -Seconds 1
  & (Join-Path $repo 'tests\fixtures\remove-test-vhd.ps1')
  Log 'test VHD removed'
  Remove-Item $ready, $done -ErrorAction SilentlyContinue
}
