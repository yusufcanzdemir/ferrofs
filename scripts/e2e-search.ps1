# End-to-end test for ferro-search against the fake_service example (no admin needed).
# Uses a throwaway %APPDATA% and %LOCALAPPDATA% (config with a custom hotkey, history,
# logs), toggles the window, checks copy path / copy file / click-to-copy-folder /
# reveal, and saves a screenshot of the window only (PrintWindow).
#   cargo build -p ferro-search --examples; cargo build -p ferro-search
#   .\scripts\e2e-search.ps1 -Repo $PWD -Out target\search.png
param([string]$Repo, [string]$Out, [string]$Query = 'app.rs')
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
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  public struct RECT { public int L, T, R, B; }
}
'@
Add-Type -AssemblyName System.Drawing
[U]::SetProcessDPIAware() | Out-Null
$wsh = New-Object -ComObject WScript.Shell
function Check($name, $ok, $detail) { "{0,-34} {1} {2}" -f $name, ($(if ($ok) { 'OK  ' } else { 'FAIL' })), $detail }
# Presses a chord like a person: modifiers down, key, a short hold, then release.
function Send-Chord([byte[]]$Keys) {
  foreach ($k in $Keys) { [U]::keybd_event($k, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 30 }
  Start-Sleep -Milliseconds 120
  [array]::Reverse($Keys)
  foreach ($k in $Keys) { [U]::keybd_event($k, 0, 2, [UIntPtr]::Zero) }
}
function Clip-Text { (Get-Clipboard -Raw) -replace '\s+$', '' }
# (Get-Clipboard -Format FileDropList cannot read a plain CF_HDROP; WinForms can.)
function Clip-Files { powershell.exe -NoProfile -STA -Command 'Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::GetFileDropList()' }

$sandbox = Join-Path $env:TEMP "ferrofs-e2e-search-$PID"
New-Item -ItemType Directory -Force "$sandbox\roaming\ferrofs", "$sandbox\local" | Out-Null
Set-Content "$sandbox\roaming\ferrofs\config.toml" "[search]`nhotkey = `"Ctrl+Alt+K`"`n"

$fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList $Repo -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$saved = @{ APPDATA = $env:APPDATA; LOCALAPPDATA = $env:LOCALAPPDATA }
$env:APPDATA = "$sandbox\roaming"; $env:LOCALAPPDATA = "$sandbox\local"; $env:FERROFS_DEV_ANY_SERVER = '1'
$ui = Start-Process (Join-Path $Repo 'target\debug\ferro-search.exe') -PassThru
$env:APPDATA = $saved.APPDATA; $env:LOCALAPPDATA = $saved.LOCALAPPDATA
Start-Sleep -Seconds 3
$h = [U]::FindWindow([NullString]::Value, 'ferrofs search')
try {
  Check 'hidden at start' (-not [U]::IsWindowVisible($h))
  $wsh.SendKeys('^% '); Start-Sleep -Milliseconds 600
  Check 'default hotkey replaced' (-not [U]::IsWindowVisible($h))
  $wsh.SendKeys('^%k'); Start-Sleep -Milliseconds 800
  Check 'configured hotkey shows' ([U]::IsWindowVisible($h) -and [U]::GetForegroundWindow() -eq $h)
  # Never type into whatever else is in front (e.g. Esc or Ctrl+Enter in the editor).
  if ([U]::GetForegroundWindow() -ne $h) { throw 'search window is not in the foreground; not sending keys' }

  $wsh.SendKeys($Query); Start-Sleep -Milliseconds 1200
  $wsh.SendKeys('^c'); Start-Sleep -Milliseconds 400
  $path = Clip-Text
  Check 'Ctrl+C copies the path' ($path -like "*\$Query") $path
  Send-Chord @(0x11, 0x10, 0x43); Start-Sleep -Milliseconds 600   # Ctrl+Shift+C
  $files = Clip-Files
  Check 'Ctrl+Shift+C copies the file' ($files -eq $path) $files

  $r = New-Object U+RECT; [U]::GetWindowRect($h, [ref]$r) | Out-Null
  $bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [U]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
  $bmp.Save($Out, [Drawing.Imaging.ImageFormat]::Png)
  # First result row, middle of the Folder column (layout proportions of the window).
  [U]::SetCursorPos($r.L + [int](($r.R - $r.L) * 0.5), $r.T + [int](($r.B - $r.T) * 0.238)) | Out-Null
  Start-Sleep -Milliseconds 300
  [U]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); [U]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 500
  $folder = Clip-Text
  Check 'click on folder copies the folder' ($folder -eq (Split-Path $path -Parent)) $folder

  $wsh.SendKeys('^{ENTER}'); Start-Sleep -Seconds 2
  Check 'Ctrl+Enter hides the window' (-not [U]::IsWindowVisible($h))
  $shell = New-Object -ComObject Shell.Application
  $revealed = @($shell.Windows() | Where-Object { $_.Document.Folder.Self.Path -eq $folder }).Count -gt 0
  Check 'Ctrl+Enter reveals in Explorer' $revealed
  $history = Get-Item "$sandbox\local\ferrofs\history.bin" -ErrorAction SilentlyContinue
  Check 'reveal is recorded in history' ($history -and $history.Length -gt 0)
  "screenshot: $Out"
} finally {
  Stop-Process -Id $ui.Id, $fake.Id -Force -ErrorAction SilentlyContinue
  Get-Content "$sandbox\local\ferrofs\logs\ferro-search*" -ErrorAction SilentlyContinue | Select-String 'WARN|ERROR|frame after toggle'
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
