# End-to-end test for the ferro-storage analyzer against the ferro-service fake_service
# example (no admin needed). Indexes -Root with the fake service, opens the analyzer at
# -Root and at -Folder in a throwaway %LOCALAPPDATA%, and saves screenshots of the window
# only (PrintWindow). Sends no keystrokes; clicks only while our window is in front.
# With -NoFake it uses an already running service instead (e.g. `run --console` started
# by e2e-service.ps1), and -Root / -Folder are real paths such as C:\ and C:\Windows.
#   cargo build -p ferro-service --examples; cargo build -p ferro-storage
#   .\scripts\e2e-storage.ps1 -Repo $PWD -Root $PWD -Folder "$PWD\crates" -Out target\storage
param([string]$Repo, [string]$Root, [string]$Folder, [string]$Out, [switch]$NoFake)
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
function Check($name, $ok, $detail) { "{0,-34} {1} {2}" -f $name, ($(if ($ok) { 'OK  ' } else { 'FAIL' })), $detail }
function Shot($h, $file) {
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  $bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [U]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
  $bmp.Save($file, [Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $bmp.Dispose()
  $file
}
# Clicks at a fraction of the window, only if it is the foreground window.
function Click-At($h, [double]$fx, [double]$fy, [switch]$Double) {
  if ([U]::GetForegroundWindow() -ne $h) { throw 'analyzer is not in the foreground; not clicking' }
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  [U]::SetCursorPos($r.L + [int](($r.R - $r.L) * $fx), $r.T + [int](($r.B - $r.T) * $fy)) | Out-Null
  Start-Sleep -Milliseconds 200
  $n = if ($Double) { 2 } else { 1 }
  for ($i = 0; $i -lt $n; $i++) { [U]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); [U]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 60 }
}
function Start-Ui($arg) {
  $saved = $env:LOCALAPPDATA
  $env:LOCALAPPDATA = "$sandbox\local"; $env:FERROFS_DEV_ANY_SERVER = '1'
  $p = Start-Process (Join-Path $Repo 'target\debug\ferro-storage.exe') -ArgumentList "`"$arg`"" -PassThru
  $env:LOCALAPPDATA = $saved
  for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 250
    $h = [U]::FindWindow([NullString]::Value, 'ferrofs storage')
    if ($h -ne [IntPtr]::Zero -and [U]::IsWindowVisible($h)) { break }
  }
  Start-Sleep -Seconds 2
  return @($p, $h)
}

$sandbox = Join-Path $env:TEMP "ferrofs-e2e-storage-$PID"
New-Item -ItemType Directory -Force "$sandbox\local" | Out-Null
New-Item -ItemType Directory -Force $Out | Out-Null
$fake = $null
if (-not $NoFake) {
  $fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList "`"$Root`"" -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 3
}
$ui = $null
try {
  $ui, $h = Start-Ui $Root
  Check 'window opens' ($h -ne [IntPtr]::Zero) $h
  Shot $h (Join-Path $Out 'root.png')
  Stop-Process -Id $ui.Id -Force; Start-Sleep -Milliseconds 500

  $ui, $h = Start-Ui $Folder
  Shot $h (Join-Path $Out 'folder.png')
  if ([U]::GetForegroundWindow() -eq $h) {
    # Extensions tab (right panel header), then its first row: filters by that extension.
    Click-At $h 0.835 0.115; Start-Sleep -Milliseconds 500
    Shot $h (Join-Path $Out 'extensions.png')
    Click-At $h 0.86 0.185; Start-Sleep -Seconds 1
    Shot $h (Join-Path $Out 'filtered.png')
  } else {
    Check 'analyzer in foreground' $false 'skipped clicks'
  }
} finally {
  if ($ui) { Stop-Process -Id $ui.Id -Force -ErrorAction SilentlyContinue }
  if ($fake) { Stop-Process -Id $fake.Id -Force -ErrorAction SilentlyContinue }
  Get-Content "$sandbox\local\ferrofs\logs\ferro-storage*" -ErrorAction SilentlyContinue | Select-String 'WARN|ERROR'
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
