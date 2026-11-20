# End-to-end test for the duplicate finder against the ferro-service fake_service example
# (no admin needed). Builds a throwaway fixture under %TEMP% (never a real user folder),
# opens `ferro-storage --duplicates <fixture>` with a throwaway %LOCALAPPDATA% (the action
# log lands there), and drives it with clicks only while our window is in front:
#   1. results, 2. Mark duplicates, 3. hard link preview -> Dry run (log only),
#   4. hard link preview -> Delete permanently -> Apply, 5. search again (links are one
#   file now), 6. new copies -> Remove marked -> Delete permanently -> Apply.
# Permanent deletion keeps the user's Recycle Bin out of the test.
#   cargo build -p ferro-service --examples; cargo build -p ferro-storage
#   .\scripts\e2e-duplicates.ps1 -Repo $PWD -Out target\duplicates
param([string]$Repo, [string]$Out)
$ErrorActionPreference = 'Stop'
# Development pipe: never talk to (or collide with) an installed ferrofs service.
$env:FERROFS_DEV_PIPE = 'ferrofs-dev'
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
# Clicks at window pixel offsets (from the window's top-left), only while it is in front.
function Click-At($h, [int]$x, [int]$y) {
  if ([U]::GetForegroundWindow() -ne $h) { throw 'storage window is not in the foreground; not clicking' }
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  [U]::SetCursorPos($r.L + $x, $r.T + $y) | Out-Null
  Start-Sleep -Milliseconds 200
  [U]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); [U]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 600
}
function Bytes([int]$len, [int]$seed) {
  $b = [byte[]]::new($len); (New-Object Random $seed).NextBytes($b); $b
}
function New-Fixture($dir) {
  New-Item -ItemType Directory -Force "$dir\a", "$dir\b\deep" | Out-Null
  $big = Bytes (3MB) 1
  [IO.File]::WriteAllBytes("$dir\a\movie.mkv", $big)
  [IO.File]::WriteAllBytes("$dir\b\movie (copy).mkv", $big)
  [IO.File]::WriteAllBytes("$dir\b\deep\movie.mkv", $big)
  $twin = $big.Clone(); $twin[1500000] = $twin[1500000] -bxor 0xFF   # same size, same edges
  [IO.File]::WriteAllBytes("$dir\b\lookalike.mkv", $twin)
  $photo = Bytes (300KB) 2
  [IO.File]::WriteAllBytes("$dir\a\photo.jpg", $photo)
  [IO.File]::WriteAllBytes("$dir\b\photo.jpg", $photo)
  $small = Bytes (10KB) 3                                             # below the minimum
  [IO.File]::WriteAllBytes("$dir\a\note.txt", $small)
  [IO.File]::WriteAllBytes("$dir\b\note.txt", $small)
  [IO.File]::WriteAllBytes("$dir\a\unique.bin", (Bytes (500KB) 4))
}
function FileId($path) { (fsutil file queryfileid $path) -replace '.*: ', '' }

$sandbox = Join-Path $env:TEMP "ferrofs-e2e-duplicates-$PID"
$fixture = "$sandbox\fixture"
New-Item -ItemType Directory -Force "$sandbox\local", $Out | Out-Null
New-Fixture $fixture
$log = "$sandbox\local\ferrofs\actions.jsonl"
$fake = $null; $ui = $null
function Start-All {
  $script:fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList "`"$fixture`"" -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 2
  $saved = $env:LOCALAPPDATA
  $env:LOCALAPPDATA = "$sandbox\local"; $env:FERROFS_DEV_ANY_SERVER = '1'
  $script:ui = Start-Process (Join-Path $Repo 'target\debug\ferro-storage.exe') -ArgumentList '--duplicates', "`"$fixture`"" -PassThru
  $env:LOCALAPPDATA = $saved
  for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 250
    $script:h = [U]::FindWindow([NullString]::Value, 'ferrofs storage')
    if ($script:h -ne [IntPtr]::Zero -and [U]::IsWindowVisible($script:h)) { break }
  }
  Start-Sleep -Seconds 3
}
function Stop-All {
  if ($script:ui) { Stop-Process -Id $script:ui.Id -Force -ErrorAction SilentlyContinue }
  if ($script:fake) { Stop-Process -Id $script:fake.Id -Force -ErrorAction SilentlyContinue }
  Start-Sleep -Milliseconds 500
}

try {
  Start-All
  Check 'window opens' ($h -ne [IntPtr]::Zero) $h
  Shot $h '1-results'
  Click-At $h 271 119                       # Mark duplicates
  Shot $h '2-marked'
  Click-At $h 740 119                       # Replace marked with hard links…
  Shot $h '3-preview'
  $ids = @{}; Get-ChildItem -Recurse -File $fixture | ForEach-Object { $ids[$_.FullName] = FileId $_.FullName }
  Click-At $h 535 699                       # Dry run (log only)
  Start-Sleep -Seconds 2
  $lines = @(Get-Content $log -ErrorAction SilentlyContinue)
  Check 'dry run logs 3 actions' ($lines.Count -eq 3 -and ($lines | Where-Object { $_ -match '"dry_run":true' }).Count -eq 3) "$($lines.Count) lines"
  $same = @(Get-ChildItem -Recurse -File $fixture | Where-Object { $ids[$_.FullName] -ne (FileId $_.FullName) }).Count -eq 0
  Check 'dry run changes nothing' $same

  Click-At $h 740 119                       # preview again
  Click-At $h 418 665                       # Delete permanently (keeps the Recycle Bin clean)
  Click-At $h 434 699                       # Apply
  Start-Sleep -Seconds 3
  Shot $h '4-linked'
  $keep = FileId "$fixture\a\movie.mkv"
  Check 'copy is a hard link now' ((FileId "$fixture\b\deep\movie.mkv") -eq $keep -and (FileId "$fixture\b\movie (copy).mkv") -eq $keep)
  Check 'photo is a hard link now' ((FileId "$fixture\b\photo.jpg") -eq (FileId "$fixture\a\photo.jpg"))
  Check 'lookalike untouched' ((FileId "$fixture\b\lookalike.mkv") -eq $ids["$fixture\b\lookalike.mkv"])
  $hash = (Get-FileHash "$fixture\a\movie.mkv").Hash
  Check 'content intact' ((Get-FileHash "$fixture\b\deep\movie.mkv").Hash -eq $hash)
  Check 'no temporary links left' (@(Get-ChildItem -Recurse -Force $fixture -Filter '*.tmp').Count -eq 0)
  $lines = @(Get-Content $log)
  Check 'apply logs 3 more actions' (($lines | Where-Object { $_ -match '"action":"hardlink-delete-original".*"result":"ok"' }).Count -eq 3) "$($lines.Count) lines"

  Click-At $h 991 92                        # Find duplicates again
  Start-Sleep -Seconds 3
  Shot $h '5-after-links'

  # New copies: the fake service only indexes at start, so restart it.
  Stop-All
  Copy-Item "$fixture\a\unique.bin" "$fixture\b\unique-copy.bin"
  Start-All
  Shot $h '6-new-copy'
  Click-At $h 271 119                       # Mark duplicates
  Click-At $h 535 119                       # Remove marked…
  Shot $h '7-remove-preview'
  Click-At $h 418 589                       # Delete permanently
  Click-At $h 434 622                       # Apply
  Start-Sleep -Seconds 2
  Shot $h '8-removed'
  Check 'copy removed' (-not (Test-Path "$fixture\b\unique-copy.bin"))
  Check 'kept copy stays' (Test-Path "$fixture\a\unique.bin")
  $lines = @(Get-Content $log)
  Check 'removal logged' (($lines[-1] -match '"action":"delete"') -and ($lines[-1] -match '"result":"ok"')) $lines[-1]
} finally {
  Stop-All
  Get-Content "$sandbox\local\ferrofs\logs\ferro-storage*" -ErrorAction SilentlyContinue | Select-String 'WARN|ERROR'
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
"failures: $failures"
