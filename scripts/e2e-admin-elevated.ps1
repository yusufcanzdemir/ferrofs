# Elevated half of e2e-admin.ps1 (started by it with a UAC prompt; not meant to be run on
# its own). Checks the instances that ferro-storage restarted as administrator, closes
# them, then drives its own elevated instances with the fixture environment. Only the
# fixture is touched. Writes its report to <Out>\elevated.txt.
param([string]$Repo, [string]$Out, [string]$Fixture)
$ErrorActionPreference = 'Stop'
$fx = $Fixture
. (Join-Path $PSScriptRoot 'e2e-ui-helpers.ps1')
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class Tok {
  [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
  [DllImport("advapi32.dll")] static extern bool OpenProcessToken(IntPtr p, uint access, out IntPtr t);
  [DllImport("advapi32.dll")] static extern bool GetTokenInformation(IntPtr t, int cls, out int value, int len, out int ret);
  // 1 = elevated, 0 = not elevated, -1 = unknown.
  public static int Elevated(uint pid) {
    IntPtr p = OpenProcess(0x1000, false, pid); if (p == IntPtr.Zero) return -1;
    IntPtr t; int value = -1, ret;
    if (OpenProcessToken(p, 0x8, out t)) { if (!GetTokenInformation(t, 20, out value, 4, out ret)) value = -1; CloseHandle(t); }
    CloseHandle(p); return value;
  }
}
'@
$script:failures = 0
$report = Join-Path $Out 'elevated.txt'
$exe = Join-Path $Repo 'target\debug\ferro-storage.exe'
$lines = New-Object Collections.Generic.List[string]
function Say($text) { $lines.Add($text); $text }

try {
  Say (Check 'helper itself is elevated' ([Tok]::Elevated([uint32]$PID) -eq 1))
  # 1. Instances restarted by the app.
  $procs = @(Get-CimInstance Win32_Process -Filter "Name = 'ferro-storage.exe'")
  # The restarted instances are the elevated ones (the unelevated originals closed themselves).
  $cleanup = $procs | Where-Object { $_.CommandLine -match '--cleanup' -and [Tok]::Elevated([uint32]$_.ProcessId) -eq 1 } | Select-Object -First 1
  $compression = $procs | Where-Object { $_.CommandLine -match '--compression' -and [Tok]::Elevated([uint32]$_.ProcessId) -eq 1 } | Select-Object -First 1
  Say ("processes: " + (($procs | ForEach-Object { "$($_.ProcessId) elevated=$([Tok]::Elevated([uint32]$_.ProcessId)) $($_.CommandLine)" }) -join ' | '))
  Say (Check 'restarted cleanup is elevated' ($cleanup -and [Tok]::Elevated([uint32]$cleanup.ProcessId) -eq 1) $cleanup.CommandLine)
  Say (Check 'restarted cleanup keeps its folder' ($cleanup.CommandLine -match [regex]::Escape("$fx\dev")))
  Say (Check 'restarted compression is elevated' ($compression -and [Tok]::Elevated([uint32]$compression.ProcessId) -eq 1) $compression.CommandLine)
  Say (Check 'restarted compression keeps its folder' ($compression.CommandLine -match [regex]::Escape("$fx\locked\App")))
  foreach ($p in @($cleanup, $compression) | Where-Object { $_ }) {
    $h = Find-Window ([int]$p.ProcessId)
    if ($h -ne [IntPtr]::Zero) { Say (Shot $h ("b-restarted-" + $(if ($p.CommandLine -match 'cleanup') { 'cleanup' } else { 'compression' }))) }
    Stop-Process -Id $p.ProcessId -Force
  }
  Start-Sleep -Seconds 1

  # 2. Elevated cleanup with the fixture environment: Windows temp becomes cleanable.
  $fake = Get-Process fake_service -ErrorAction SilentlyContinue | Select-Object -First 1
  Say (Check 'fake service still running' ($null -ne $fake))
  $ui = Start-Fixture-Ui $exe @('--cleanup', "`"$fx\dev`"") $fx
  $h = Wait-Window $ui.Id
  Say (Shot $h 'b3-cleanup-elevated')
  Stop-Process -Id $ui.Id -Force
  Start-Sleep -Seconds 1

  # 3. Elevated compression of the locked folder, then undo.
  $app = "$fx\locked\App"
  $before = OnDisk $app
  $hash = (Get-FileHash "$app\bin\core.dll").Hash
  $ui = Start-Fixture-Ui $exe @('--compression', "`"$app`"") $fx
  $h = Wait-Window $ui.Id
  Say (Shot $h 'b4-compression-elevated')
  Click-At $h 802 92                        # Compress…
  Click-At $h 458 623                       # Apply
  Start-Sleep -Seconds 4
  Say (Shot $h 'b5-compressed')
  $after = OnDisk $app
  Say (Check 'locked files compressed as admin' ($after -lt $before / 4) "$before -> $after")
  Say (Check 'contents unchanged' ((Get-FileHash "$app\bin\core.dll").Hash -eq $hash))
  Click-At $h 917 92                        # Decompress…
  Click-At $h 458 601                       # Apply
  Start-Sleep -Seconds 4
  Say (Check 'sizes restored' ((OnDisk $app) -eq $before) "$(OnDisk $app) vs $before")
  $log = @(Get-Content "$fx\local\ferrofs\actions.jsonl" -ErrorAction SilentlyContinue)
  Say (Check 'both runs logged in the fixture' ($log.Count -eq 2 -and $log[0] -match '"compress"' -and $log[1] -match '"decompress"') "$($log.Count) lines")
  Stop-Process -Id $ui.Id -Force
} catch {
  Say "ERROR: $_"
  $script:failures++
} finally {
  # Give the folder back to normal inheritance so the unelevated script can delete it.
  icacls "$fx\locked" /reset /T /Q | Out-Null
  Say "failures (elevated): $failures"
  $lines | Set-Content $report
}
