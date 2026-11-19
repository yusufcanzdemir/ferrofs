# End-to-end smoke test for ferro-link: simulates hotkeys in Explorer and Notepad,
# then closes the app via WM_CLOSE. Opens windows on the desktop; run it manually:
#   cargo build -p ferro-link; .\scripts\e2e-link.ps1 -Exe .\target\debug\ferro-link.exe
param([string]$Exe)
$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
  public static string FgClass() { var sb = new System.Text.StringBuilder(256); GetClassName(GetForegroundWindow(), sb, 256); return sb.ToString(); }
}
'@
$wsh = New-Object -ComObject WScript.Shell
$logDir = "$env:LOCALAPPDATA\ferrofs\logs"
$env:FERROFS_LOG = 'debug'

$dir = Join-Path $env:TEMP "ferrofs-e2e-$PID"
New-Item -ItemType Directory -Force $dir | Out-Null
$file = Join-Path $dir 'e2e-picked.txt'
Set-Content $file 'x'

$app = Start-Process -FilePath $Exe -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2

# 1) Explorer: Ctrl+B should be consumed and produce a pick
Start-Process explorer.exe "/select,`"$file`""
Start-Sleep -Seconds 3
"foreground before Ctrl+B in Explorer: $([W]::FgClass())"
$wsh.SendKeys('^b')
Start-Sleep -Seconds 2

# 2) Notepad: Ctrl+B must pass through (no pick)
$np = Start-Process notepad.exe -PassThru
Start-Sleep -Seconds 3
"foreground before Ctrl+B in Notepad: $([W]::FgClass())"
$wsh.SendKeys('^b')
Start-Sleep -Seconds 1
Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue

# 3) Graceful shutdown via WM_CLOSE
$h = [W]::FindWindow('ferrofs.link.tray', [NullString]::Value)
[W]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
$exited = $app.WaitForExit(5000)
"graceful exit: $exited"
if (-not $exited) { Stop-Process -Id $app.Id -Force }

Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
"--- log ---"
Get-ChildItem $logDir | Sort-Object LastWriteTime | Select-Object -Last 1 | Get-Content -Tail 12
