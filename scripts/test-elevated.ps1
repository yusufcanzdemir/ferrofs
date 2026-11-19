# Runs the tests that need administrator rights: creates the test VHD, runs the ignored
# ferro-core tests on it, removes the VHD, then does a read-only cold scan of C:\ to
# measure index build time. Output goes to target\elevated-test.log.
#   Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"$PWD\scripts\test-elevated.ps1"
$ErrorActionPreference = 'Continue'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo
$log = Join-Path $repo 'target\elevated-test.log'
"=== $(Get-Date -Format s) ===" | Set-Content $log

try {
  $letter = & (Join-Path $repo 'tests\fixtures\new-test-vhd.ps1')
  "test VHD mounted at ${letter}:" | Add-Content $log
  $env:FERROFS_TEST_VOLUME = "${letter}:\"
  cargo test -q --release -p ferro-core --test usn_vhd -- --ignored --nocapture --test-threads=1 *>> $log
  "usn_vhd exit code: $LASTEXITCODE" | Add-Content $log
  cargo test -q --release -p ferro-core --test mft_vhd -- --ignored --nocapture --test-threads=1 *>> $log
  "mft_vhd exit code: $LASTEXITCODE" | Add-Content $log
} catch {
  "ERROR: $_" | Add-Content $log
} finally {
  & (Join-Path $repo 'tests\fixtures\remove-test-vhd.ps1')
  "test VHD removed" | Add-Content $log
}

"--- USN enumeration ---" | Add-Content $log
$env:FERROFS_SCAN_MODE = 'usn'
cargo run -q --release -p ferro-core --example scan -- C:\ *>> $log
Remove-Item Env:FERROFS_SCAN_MODE
"--- raw MFT ---" | Add-Content $log
$env:FERROFS_SCAN_COMPARE = '1'
cargo run -q --release -p ferro-core --example scan -- C:\ report "ext:pdf" "*.rs" "path:windows\system32 dll" *>> $log
"scan exit code: $LASTEXITCODE" | Add-Content $log
