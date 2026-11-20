# Shared helpers for the UI end-to-end scripts (dot-source this file). Screenshots use
# PrintWindow on our window only; clicks happen only while that window is in front.
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class U {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] public static extern uint GetCompressedFileSizeW(string name, out uint high);
  public struct RECT { public int L, T, R, B; }
}
'@
Add-Type -AssemblyName System.Drawing
[U]::SetProcessDPIAware() | Out-Null

function Check($name, $ok, $detail) {
  if (-not $ok) { $script:failures++ }
  "{0,-44} {1} {2}" -f $name, ($(if ($ok) { 'OK  ' } else { 'FAIL' })), $detail
}
# The visible "ferrofs storage" window of process $procId, or zero.
function Find-Window([int]$procId) {
  $script:found = [IntPtr]::Zero
  $cb = [U+EnumProc]{
    param($h, $l)
    $p = [uint32]0; [U]::GetWindowThreadProcessId($h, [ref]$p) | Out-Null
    $sb = New-Object Text.StringBuilder 64; [U]::GetWindowText($h, $sb, 64) | Out-Null
    if ($p -eq $procId -and $sb.ToString() -eq 'ferrofs storage' -and [U]::IsWindowVisible($h)) { $script:found = $h; return $false }
    $true
  }
  [U]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
  $script:found
}
function Wait-Window([int]$procId, [int]$seconds = 15) {
  for ($i = 0; $i -lt $seconds * 4; $i++) {
    $h = Find-Window $procId
    if ($h -ne [IntPtr]::Zero) { Start-Sleep -Seconds 3; return $h }
    Start-Sleep -Milliseconds 250
  }
  [IntPtr]::Zero
}
# Id of a ferro-storage process that is not in $known, waiting up to $seconds (e.g. for
# the user to approve a UAC prompt), or $null.
function Wait-New-Process($known, [int]$seconds) {
  for ($i = 0; $i -lt $seconds * 2; $i++) {
    $new = Get-Process ferro-storage -ErrorAction SilentlyContinue | Where-Object { $known -notcontains $_.Id } | Select-Object -First 1
    if ($new -and (Find-Window $new.Id) -ne [IntPtr]::Zero) { Start-Sleep -Seconds 3; return $new.Id }
    Start-Sleep -Milliseconds 500
  }
  $null
}
function Shot($h, $name) {
  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  $bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [U]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
  $file = Join-Path $Out "$name.png"
  $bmp.Save($file, [Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $bmp.Dispose()
  "screenshot: $file"
}
# Clicks at window pixel offsets, only while the window is in front.
function Click-At($h, [int]$x, [int]$y) {
  if ([U]::GetForegroundWindow() -ne $h) {
    [U]::SetForegroundWindow($h) | Out-Null
    Start-Sleep -Milliseconds 400
    if ([U]::GetForegroundWindow() -ne $h) { throw 'storage window is not in the foreground; not clicking' }
  }
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
function Text([int]$mb, [string]$seed) {
  $line = [Text.Encoding]::UTF8.GetBytes("<entry key=`"$seed`" value=`"some repeated configuration text`" />`r`n")
  $b = [byte[]]::new($mb * 1MB)
  for ($i = 0; $i -lt $b.Length; $i++) { $b[$i] = $line[$i % $line.Length] }
  $b
}
function OnDisk($dir) {
  $total = [uint64]0
  foreach ($f in Get-ChildItem -Recurse -File $dir) {
    $high = [uint32]0; $low = [U]::GetCompressedFileSizeW($f.FullName, [ref]$high)
    $total += ([uint64]$high -shl 32) + $low
  }
  $total
}
# Starts ferro-storage with the fixture as TEMP, LOCALAPPDATA, USERPROFILE and WINDIR.
# Never SystemRoot: Windows needs it to load system DLLs (the process does not start).
function Start-Fixture-Ui($exe, $arguments, $fx) {
  $names = 'TEMP', 'TMP', 'LOCALAPPDATA', 'USERPROFILE', 'WINDIR', 'SystemDrive', 'CARGO_HOME', 'GRADLE_USER_HOME', 'FERROFS_DEV_ANY_SERVER'
  $saved = @{}; foreach ($n in $names) { $saved[$n] = [Environment]::GetEnvironmentVariable($n) }
  $env:TEMP = "$fx\tmp"; $env:TMP = "$fx\tmp"; $env:LOCALAPPDATA = "$fx\local"; $env:USERPROFILE = "$fx\profile"
  $env:WINDIR = "$fx\windows"; $env:SystemDrive = "$fx\drive"
  Remove-Item Env:CARGO_HOME, Env:GRADLE_USER_HOME -ErrorAction SilentlyContinue
  $env:FERROFS_DEV_ANY_SERVER = '1'
  $p = Start-Process $exe -ArgumentList $arguments -PassThru
  foreach ($n in $names) { [Environment]::SetEnvironmentVariable($n, $saved[$n]) }
  $p
}
