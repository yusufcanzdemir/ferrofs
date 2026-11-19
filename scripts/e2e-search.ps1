# End-to-end smoke test for ferro-search against the fake_service example (no admin
# needed): starts both, toggles the window with Ctrl+Alt+Space, types keys, saves a
# screenshot of the window only (PrintWindow) and checks that Esc hides it.
#   cargo build -p ferro-search --examples; cargo build -p ferro-search
#   .\scripts\e2e-search.ps1 -Repo $PWD -Out target\search.png -Keys 'report'
param([string]$Repo, [string]$Out, [string[]]$Keys = @('fold'))
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
  public struct RECT { public int L, T, R, B; }
}
'@
Add-Type -AssemblyName System.Drawing
[U]::SetProcessDPIAware() | Out-Null
$wsh = New-Object -ComObject WScript.Shell

$fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList $Repo -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$env:FERROFS_DEV_ANY_SERVER = '1'
$ui = Start-Process (Join-Path $Repo 'target\debug\ferro-search.exe') -PassThru
Start-Sleep -Seconds 3
$h = [U]::FindWindow([NullString]::Value, 'ferrofs search')
"window found: $($h -ne [IntPtr]::Zero), visible before hotkey: $([U]::IsWindowVisible($h))"
$wsh.SendKeys('^% ')
Start-Sleep -Milliseconds 800
"visible after hotkey: $([U]::IsWindowVisible($h)), foreground is search: $([U]::GetForegroundWindow() -eq $h)"
foreach ($k in $Keys) { $wsh.SendKeys($k); Start-Sleep -Milliseconds 900 }
$r = New-Object U+RECT
[U]::GetWindowRect($h, [ref]$r) | Out-Null
$bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc(); [U]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
$bmp.Save($Out, [Drawing.Imaging.ImageFormat]::Png)
"screenshot: $Out ($($bmp.Width)x$($bmp.Height))"
$wsh.SendKeys('{ESC}')
Start-Sleep -Milliseconds 500
"visible after Esc: $([U]::IsWindowVisible($h))"
Stop-Process -Id $ui.Id, $fake.Id -Force
