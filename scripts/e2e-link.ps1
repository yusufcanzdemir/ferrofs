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

# Isolated %APPDATA% with a custom pick hotkey; the real config is never touched.
$appData = Join-Path $dir 'appdata'
New-Item -ItemType Directory -Force (Join-Path $appData 'ferrofs') | Out-Null
Set-Content (Join-Path $appData 'ferrofs\config.toml') "[hotkeys]`npick = `"Ctrl+Alt+P`"`n"
$realAppData = $env:APPDATA
$env:APPDATA = $appData
$app = Start-Process -FilePath $Exe -PassThru -WindowStyle Hidden
$env:APPDATA = $realAppData
Start-Sleep -Seconds 2

function Get-LinkInfo([string]$name) {
  $item = Get-Item (Join-Path $target $name) -ErrorAction SilentlyContinue
  if ($item) { "$($item.LinkType) -> $($item.Target)" } else { 'missing' }
}

# 1) The default Ctrl+B is replaced by the config, so it must not pick anything
Start-Process explorer.exe "/select,`"$file`""
Send-Hotkey '^b' 'Explorer (file selected, old hotkey)'
Start-Process explorer.exe "`"$target`""
Send-Hotkey '^g' 'Explorer (target folder)'
"after old hotkey: $(Get-LinkInfo 'e2e-picked.txt')"

# 2) Configured Ctrl+Alt+P picks the file, Ctrl+G creates the default symlink
Start-Process explorer.exe "/select,`"$file`""
Send-Hotkey '^%p' 'Explorer (file selected)'
Start-Process explorer.exe "`"$target`""
Send-Hotkey '^g' 'Explorer (target folder)'
"file link: $(Get-LinkInfo 'e2e-picked.txt')"

# 3) Shift+Ctrl+G opens the link type menu: Esc cancels, H makes a hard link
Send-Hotkey '+^g' 'Explorer (target folder, menu)'
$wsh.SendKeys('{ESC}')
Start-Sleep -Seconds 1
"after Esc: $(Get-LinkInfo 'e2e-picked (link).txt')"
Send-Hotkey '+^g' 'Explorer (target folder, menu)'
$wsh.SendKeys('h')
Start-Sleep -Seconds 2
"hard link via menu: $(Get-LinkInfo 'e2e-picked (link).txt')"

# 4) Folder: default drop makes a junction, the menu's S makes a folder symlink
Start-Process explorer.exe "/select,`"$srcDir`""
Send-Hotkey '^%p' 'Explorer (folder selected)'
Start-Process explorer.exe "`"$target`""
Send-Hotkey '^g' 'Explorer (target folder)'
"folder link: $(Get-LinkInfo 'picked-folder')"
"read through junction: $(Get-Content (Join-Path $target 'picked-folder\inner.txt') -ErrorAction SilentlyContinue)"
Send-Hotkey '+^g' 'Explorer (target folder, menu)'
$wsh.SendKeys('s')
Start-Sleep -Seconds 2
"folder symlink via menu: $(Get-LinkInfo 'picked-folder (link)')"

# 5) Notepad: the pick hotkey must pass through (no pick)
$np = Start-Process notepad.exe -PassThru
Start-Sleep -Seconds 3
"foreground before Ctrl+Alt+P in Notepad: $([W]::FgClass())"
$wsh.SendKeys('^%p')
Start-Sleep -Seconds 1
Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue

# 6) Graceful shutdown via WM_CLOSE
$h = [W]::FindWindow('ferrofs.link.tray', [NullString]::Value)
[W]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
$exited = $app.WaitForExit(5000)
"graceful exit: $exited"
if (-not $exited) { Stop-Process -Id $app.Id -Force }

Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
"--- log ---"
Get-ChildItem $logDir | Sort-Object LastWriteTime | Select-Object -Last 1 | Get-Content -Tail 20
