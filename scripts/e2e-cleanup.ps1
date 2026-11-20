# End-to-end test for the cleanup tool against the ferro-service fake_service example (no
# admin needed). Everything happens in a throwaway fixture under %TEMP%: the UI gets TEMP,
# LOCALAPPDATA and USERPROFILE pointing into it, so its "temporary files" and "npm cache"
# are fixture folders and the action log lands there too. Clicks only while our window is
# in front. Steps: 1. scan (temp selected by default, unmarked project not listed),
# 2. select the build output, 3. preview -> Dry run, 4. preview -> Delete permanently ->
# Apply (permanent keeps the user's Recycle Bin out of the test).
# The fake service has no journal, so sizes after the cleanup are checked on disk. With
# -NoFake an already running service is used instead (`run --console` from e2e-service.ps1):
# put the fixture on the test VHD with -Base (e.g. Z:\ from e2e-service.ps1; %TEMP% is
# under AppData, which the scanner deliberately ignores), and the rescan after the cleanup
# reflects the journal.
# WINDIR / SystemDrive also point into the fixture, so no real Windows location is listed.
#   cargo build -p ferro-service --examples; cargo build -p ferro-storage
#   .\scripts\e2e-cleanup.ps1 -Repo $PWD -Out target\cleanup
param([string]$Repo, [string]$Out, [switch]$ShotOnly, [switch]$NoFake, [string]$Base = $env:TEMP)
$ErrorActionPreference = 'Stop'
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class U {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, UIntPtr e);
  public struct RECT { public int L, T, R, B; }
}
'@
Add-Type -AssemblyName System.Drawing
[U]::SetProcessDPIAware() | Out-Null
$script:failures = 0
function Check($name, $ok, $detail) {
  if (-not $ok) { $script:failures++ }
  "{0,-40} {1} {2}" -f $name, ($(if ($ok) { 'OK  ' } else { 'FAIL' })), $detail
}
function Shot($h, $name) {
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  $bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [U]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
  $file = Join-Path $Out "$name.png"
  $bmp.Save($file, [Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $bmp.Dispose()
  "screenshot: $file"
}
# Clicks at window pixel offsets, only while our window is in front.
function Click-At($h, [int]$x, [int]$y) {
  if ([U]::GetForegroundWindow() -ne $h) { throw 'storage window is not in the foreground; not clicking' }
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  [U]::SetCursorPos($r.L + $x, $r.T + $y) | Out-Null
  Start-Sleep -Milliseconds 200
  [U]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); [U]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 600
}
function Fill($path, [int]$mb) {
  New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
  $b = [byte[]]::new($mb * 1MB); (New-Object Random 7).NextBytes($b); [IO.File]::WriteAllBytes($path, $b)
}

$sandbox = Join-Path $Base "ferrofs-e2e-cleanup-$PID"
$fx = "$sandbox\fixture"
New-Item -ItemType Directory -Force $Out, "$fx\profile" | Out-Null
Set-Content "$fx\dev\web\package.json" '{}' -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$fx\dev\web", "$fx\dev\app", "$fx\dev\notes" | Out-Null
Set-Content "$fx\dev\web\package.json" '{}'
Set-Content "$fx\dev\web\package-lock.json" '{}'
Fill "$fx\dev\web\node_modules\left-pad\index.js" 2
Set-Content "$fx\dev\app\Cargo.toml" '[package]'
Fill "$fx\dev\app\target\debug\app.exe" 3
Set-Content "$fx\dev\notes\README.md" 'notes'
Fill "$fx\dev\notes\target\keep.bin" 2           # no marker: must not be listed
Fill "$fx\tmp\setup-123.tmp" 1
Fill "$fx\tmp\cache\blob.bin" 1
Fill "$fx\local\npm-cache\_cacache\content.bin" 1
$log = "$fx\local\ferrofs\actions.jsonl"

$fake = $null
if ($NoFake) {
  Start-Sleep -Seconds 3   # let the service see the new files in the journal
} else {
  $fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList "`"$fx`"" -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 2
}
$saved = @{ TEMP = $env:TEMP; TMP = $env:TMP; LOCALAPPDATA = $env:LOCALAPPDATA; USERPROFILE = $env:USERPROFILE; CARGO_HOME = $env:CARGO_HOME; GRADLE_USER_HOME = $env:GRADLE_USER_HOME; WINDIR = $env:WINDIR; SystemDrive = $env:SystemDrive }
$env:TEMP = "$fx\tmp"; $env:TMP = "$fx\tmp"; $env:LOCALAPPDATA = "$fx\local"; $env:USERPROFILE = "$fx\profile"
$env:WINDIR = "$fx\windows"; $env:SystemDrive = "$fx\drive"
Remove-Item Env:CARGO_HOME, Env:GRADLE_USER_HOME -ErrorAction SilentlyContinue
$env:FERROFS_DEV_ANY_SERVER = '1'
$ui = Start-Process (Join-Path $Repo 'target\debug\ferro-storage.exe') -ArgumentList '--cleanup', "`"$fx\dev`"" -PassThru
foreach ($k in $saved.Keys) { if ($saved[$k]) { Set-Item "Env:$k" $saved[$k] } }
for ($i = 0; $i -lt 40; $i++) {
  Start-Sleep -Milliseconds 250
  $h = [U]::FindWindow([NullString]::Value, 'ferrofs storage')
  if ($h -ne [IntPtr]::Zero -and [U]::IsWindowVisible($h)) { break }
}
Start-Sleep -Seconds 3
try {
  Check 'window opens' ($h -ne [IntPtr]::Zero) $h
  Shot $h '1-scan'
  Click-At $h 200 306                       # Rust build output
  Click-At $h 200 340                       # node_modules
  Click-At $h 888 92                        # Clean selected…
  Shot $h '2-preview'
  if (-not $ShotOnly) {
    Click-At $h 510 693                     # Dry run (log only)
    Start-Sleep -Seconds 2
    $lines = @(Get-Content $log -ErrorAction SilentlyContinue)
    Check 'dry run logs 3 locations' (($lines | Where-Object { $_ -match '"tool":"cleanup".*"dry_run":true' }).Count -eq 3) "$($lines.Count) lines"
    Check 'dry run changes nothing' ((Test-Path "$fx\dev\app\target\debug\app.exe") -and (Test-Path "$fx\tmp\setup-123.tmp"))

    Click-At $h 888 92                      # Clean selected… again
    Click-At $h 393 659                     # Delete permanently
    Click-At $h 408 693                     # Apply
    Start-Sleep -Seconds 4
    Shot $h '3-cleaned'
    Check 'temp folder kept but emptied' ((Test-Path "$fx\tmp") -and @(Get-ChildItem -Force "$fx\tmp").Count -eq 0)
    Check 'Rust target removed' (-not (Test-Path "$fx\dev\app\target"))
    Check 'node_modules removed' (-not (Test-Path "$fx\dev\web\node_modules"))
    Check 'project files kept' ((Test-Path "$fx\dev\app\Cargo.toml") -and (Test-Path "$fx\dev\web\package.json"))
    Check 'unmarked target untouched' (Test-Path "$fx\dev\notes\target\keep.bin")
    Check 'unselected cache untouched' (Test-Path "$fx\local\npm-cache\_cacache\content.bin")
    $lines = @(Get-Content $log)
    $ok = @($lines | Where-Object { $_ -match '"tool":"cleanup".*"action":"delete".*"dry_run":false.*"result":"ok"' })
    Check 'cleanup logged' ($ok.Count -eq 3) "$($lines.Count) lines"
  }
} finally {
  Stop-Process -Id $ui.Id -Force -ErrorAction SilentlyContinue
  if ($fake) { Stop-Process -Id $fake.Id -Force -ErrorAction SilentlyContinue }
  Start-Sleep -Milliseconds 500
  Get-Content "$fx\local\ferrofs\logs\ferro-storage*" -ErrorAction SilentlyContinue | Select-String 'WARN|ERROR'
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
"failures: $failures"
