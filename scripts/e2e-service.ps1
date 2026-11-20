# End-to-end test of ferro-service in console mode. Run elevated:
#   Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"$PWD\scripts\e2e-service.ps1"
# On the test VHD: search after the initial MFT scan, live size refresh, folder sizes
# (SizeTree, also timed on C:\), then a restart from the snapshot with changes made while
# the service was down (journal replay).
# Writes target\e2e-service.ready and waits (up to 120 s) for target\e2e-service.done so an
# unelevated shell can query the pipe too. Log: target\e2e-service.log
$ErrorActionPreference = 'Continue'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo
$exe = Join-Path $repo 'target\release\ferro-service.exe'
$log = Join-Path $repo 'target\e2e-service.log'
$ready = Join-Path $repo 'target\e2e-service.ready'
$done = Join-Path $repo 'target\e2e-service.done'
$snapshots = Join-Path $env:ProgramData 'ferrofs\index'
$serviceLog = Join-Path $env:ProgramData 'ferrofs\logs'
Remove-Item $ready, $done -ErrorAction SilentlyContinue
"=== $(Get-Date -Format s) ===" | Set-Content $log
function Log($text) { $text | Add-Content $log }
function Start-Service-Console {
  $p = Start-Process $exe -ArgumentList 'run', '--console' -PassThru -WindowStyle Hidden `
    -RedirectStandardError (Join-Path $repo 'target\e2e-service.stderr.log')
  $deadline = (Get-Date).AddSeconds(90)
  do {
    Start-Sleep -Milliseconds 500
    $status = & $exe status 2>&1 | Out-String
  } until (($status -match 'Ready') -and ($status -notmatch 'Scanning') -or (Get-Date) -gt $deadline)
  Log "--- status ---`n$status"
  $p
}
function Query($text) { Log "--- query $text ---"; & $exe query $text *>> $log }
function Sizes($folder) { Log "--- sizes $folder ---"; & $exe sizes $folder *>> $log }

$service = $null
$before = Get-Date
try {
  $letter = & (Join-Path $repo 'tests\fixtures\new-test-vhd.ps1')
  $root = "${letter}:\"
  Log "test VHD at $root"
  New-Item -ItemType Directory "${root}e2e" | Out-Null
  Set-Content "${root}e2e\before-start.txt" 'x'

  $service = Start-Service-Console
  Query 'before-start'

  # Live size refresh: the journal only says the file changed; the service re-reads it.
  [IO.File]::WriteAllBytes("${root}e2e\grow.bin", [byte[]]::new(10))
  Start-Sleep -Milliseconds 500
  [IO.File]::WriteAllBytes("${root}e2e\grow.bin", [byte[]]::new(5000))
  Start-Sleep -Seconds 2
  Query 'grow.bin'
  Query "size:>4kb path:${root}e2e"
  Query 'dm:today grow'
  Sizes "${root}e2e"
  Sizes 'C:\'
  Sizes 'C:\Windows'
  Log '--- dupes C:\ ---'; & $exe dupes 'C:\' *>> $log

  # Restart from the snapshot; changes made while the service is down come from the journal.
  Stop-Process -Id $service.Id -Force
  Start-Sleep -Seconds 1
  Set-Content "${root}e2e\while-stopped.txt" 'x'
  Remove-Item "${root}e2e\before-start.txt"
  $service = Start-Service-Console
  Query 'while-stopped'
  Query 'before-start'

  Log '--- service log (this run) ---'
  Get-ChildItem $serviceLog -Filter 'ferro-service*' | Sort-Object LastWriteTime | Select-Object -Last 1 |
    Get-Content | Where-Object { $_ -match 'snapshot|MFT scanned|volume ready|WARN|ERROR' } |
    Where-Object { [datetime]::Parse($_.Substring(0, 19)) -ge $before.ToUniversalTime().AddSeconds(-5) } |
    Add-Content $log
  Log '--- snapshot folder ---'
  Get-ChildItem $snapshots | ForEach-Object { "{0,12:N0} {1}" -f $_.Length, $_.Name } | Add-Content $log
  icacls $snapshots | Add-Content $log

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
  # Snapshots written by this test are not needed afterwards.
  Remove-Item "$snapshots\*" -ErrorAction SilentlyContinue
  Remove-Item $ready, $done -ErrorAction SilentlyContinue
}
