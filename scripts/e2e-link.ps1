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
$srcDir = Join-Path $dir 'picked-folder'
New-Item -ItemType Directory -Force $srcDir | Out-Null
Set-Content (Join-Path $srcDir 'inner.txt') 'inner'
$target = Join-Path $dir 'target'
New-Item -ItemType Directory -Force $target | Out-Null

function Send-Hotkey([string]$keys, [string]$where) {
  Start-Sleep -Seconds 3
  "foreground before $keys in ${where}: $([W]::FgClass())"
  $wsh.SendKeys($keys)
  Start-Sleep -Seconds 2
}

$app = Start-Process -FilePath $Exe -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2

# 1) Explorer: Ctrl+B picks a file, Ctrl+G in another folder creates a symlink
Start-Process explorer.exe "/select,`"$file`""
Send-Hotkey '^b' 'Explorer (file selected)'
Start-Process explorer.exe "`"$target`""
Send-Hotkey '^g' 'Explorer (target folder)'
$fileLink = Get-Item (Join-Path $target 'e2e-picked.txt') -ErrorAction SilentlyContinue
"file link: $($fileLink.LinkType) -> $($fileLink.Target)"

# 2) Folder: Ctrl+B on a folder, Ctrl+G creates a junction
Start-Process explorer.exe "/select,`"$srcDir`""
Send-Hotkey '^b' 'Explorer (folder selected)'
Start-Process explorer.exe "`"$target`""
Send-Hotkey '^g' 'Explorer (target folder)'
$dirLink = Get-Item (Join-Path $target 'picked-folder') -ErrorAction SilentlyContinue
"folder link: $($dirLink.LinkType) -> $($dirLink.Target)"
"read through junction: $(Get-Content (Join-Path $target 'picked-folder\inner.txt') -ErrorAction SilentlyContinue)"

# 3) Notepad: Ctrl+B must pass through (no pick)
$np = Start-Process notepad.exe -PassThru
Start-Sleep -Seconds 3
"foreground before Ctrl+B in Notepad: $([W]::FgClass())"
$wsh.SendKeys('^b')
Start-Sleep -Seconds 1
Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue

# 4) Graceful shutdown via WM_CLOSE
$h = [W]::FindWindow('ferrofs.link.tray', [NullString]::Value)
[W]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
$exited = $app.WaitForExit(5000)
"graceful exit: $exited"
if (-not $exited) { Stop-Process -Id $app.Id -Force }

Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
"--- log ---"
Get-ChildItem $logDir | Sort-Object LastWriteTime | Select-Object -Last 1 | Get-Content -Tail 14
