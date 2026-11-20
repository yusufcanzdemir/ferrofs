# End-to-end test for the compression tool (no admin needed). Builds a throwaway "App"
# folder under %TEMP% (compressible files, an .mp4 and tiny files), opens
# `ferro-storage --compression <folder>` with a throwaway %LOCALAPPDATA% (action log) and
# the fake service, and clicks only while our window is in front:
#   1. analysis + estimate, 2. Compress -> Dry run, 3. Compress -> Apply (sizes on disk
#   shrink, contents unchanged), 4. Decompress -> Apply (sizes back).
#   cargo build -p ferro-service --examples; cargo build -p ferro-storage
#   .\scripts\e2e-compression.ps1 -Repo $PWD -Out target\compression
param([string]$Repo, [string]$Out, [switch]$ShotOnly)
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
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] public static extern uint GetCompressedFileSizeW(string name, out uint high);
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
function Click-At($h, [int]$x, [int]$y) {
  if ([U]::GetForegroundWindow() -ne $h) { throw 'storage window is not in the foreground; not clicking' }
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  [U]::SetCursorPos($r.L + $x, $r.T + $y) | Out-Null
  Start-Sleep -Milliseconds 200
  [U]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); [U]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 600
}
function OnDisk($dir) {
  $total = [uint64]0
  foreach ($f in Get-ChildItem -Recurse -File $dir) {
    $high = [uint32]0; $low = [U]::GetCompressedFileSizeW($f.FullName, [ref]$high)
    $total += ([uint64]$high -shl 32) + $low
  }
  $total
}
function Text([int]$mb, [string]$seed) {
  $line = [Text.Encoding]::UTF8.GetBytes("<entry key=`"$seed`" value=`"some repeated configuration text`" />`r`n")
  $b = [byte[]]::new($mb * 1MB)
  for ($i = 0; $i -lt $b.Length; $i++) { $b[$i] = $line[$i % $line.Length] }
  $b
}

$sandbox = Join-Path $env:TEMP "ferrofs-e2e-compression-$PID"
$app = "$sandbox\App"
New-Item -ItemType Directory -Force "$app\bin", "$app\data", "$sandbox\local", $Out | Out-Null
[IO.File]::WriteAllBytes("$app\bin\core.dll", (Text 8 'core'))
[IO.File]::WriteAllBytes("$app\bin\ui.dll", (Text 4 'ui'))
[IO.File]::WriteAllBytes("$app\data\levels.dat", (Text 6 'levels'))
$noise = [byte[]]::new(2MB); (New-Object Random 5).NextBytes($noise)
[IO.File]::WriteAllBytes("$app\data\intro.mp4", $noise)
Set-Content "$app\settings.ini" '[x]'
$hash = (Get-FileHash "$app\bin\core.dll").Hash
$log = "$sandbox\local\ferrofs\actions.jsonl"
$before = OnDisk $app

$fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList "`"$sandbox`"" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$saved = $env:LOCALAPPDATA
$env:LOCALAPPDATA = "$sandbox\local"; $env:FERROFS_DEV_ANY_SERVER = '1'
$ui = Start-Process (Join-Path $Repo 'target\debug\ferro-storage.exe') -ArgumentList '--compression', "`"$app`"" -PassThru
$env:LOCALAPPDATA = $saved
for ($i = 0; $i -lt 40; $i++) {
  Start-Sleep -Milliseconds 250
  $h = [U]::FindWindow([NullString]::Value, 'ferrofs storage')
  if ($h -ne [IntPtr]::Zero -and [U]::IsWindowVisible($h)) { break }
}
Start-Sleep -Seconds 3
try {
  Check 'window opens' ($h -ne [IntPtr]::Zero) $h
  Shot $h '1-analysis'
  Click-At $h 802 92                        # Compress…
  Shot $h '2-preview'
  if (-not $ShotOnly) {
    Click-At $h 560 623                     # Dry run (log only)
    Start-Sleep -Seconds 2
    $lines = @(Get-Content $log -ErrorAction SilentlyContinue)
    Check 'dry run logged' ($lines.Count -eq 1 -and $lines[0] -match '"tool":"compression".*"dry_run":true') "$($lines.Count) lines"
    Check 'dry run changes nothing' ((OnDisk $app) -eq $before) "$(OnDisk $app) vs $before"

    Click-At $h 802 92                      # Compress…
    Click-At $h 458 623                     # Apply
    Start-Sleep -Seconds 4
    Shot $h '3-compressed'
    $after = OnDisk $app
    Check 'files take less space' ($after -lt $before / 4) "$before -> $after"
    Check 'contents unchanged' ((Get-FileHash "$app\bin\core.dll").Hash -eq $hash)
    $lines = @(Get-Content $log)
    Check 'compression logged' ($lines[-1] -match '"action":"compress".*"changed":3.*"result":"ok"') $lines[-1]

    Click-At $h 917 92                      # Decompress…
    Shot $h '4-decompress-preview'
    Click-At $h 458 601                     # Apply
    Start-Sleep -Seconds 4
    Shot $h '5-decompressed'
    Check 'sizes restored' ((OnDisk $app) -eq $before) "$(OnDisk $app) vs $before"
    Check 'contents still unchanged' ((Get-FileHash "$app\bin\core.dll").Hash -eq $hash)
    $lines = @(Get-Content $log)
    Check 'decompression logged' ($lines[-1] -match '"action":"decompress".*"changed":3.*"result":"ok"') $lines[-1]
  }
} finally {
  Stop-Process -Id $ui.Id, $fake.Id -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
  Get-Content "$sandbox\local\ferrofs\logs\ferro-storage*" -ErrorAction SilentlyContinue | Select-String 'WARN|ERROR'
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
"failures: $failures"
