# Takes the README screenshots from a synthetic demo disk, so no real file names or user
# paths appear in them. Builds a demo tree under %TEMP%, maps it to drive X: with `subst`
# (no admin needed), serves it with the fake service and captures the windows with
# PrintWindow. Large files are sparse: they show gigabytes but take almost no disk space.
#   cargo build -p ferro-service --examples; cargo build -p ferro-storage -p ferro-search
#   .\scripts\readme-screenshots.ps1 -Repo $PWD
param([string]$Repo, [string]$Drive = 'X:')
$ErrorActionPreference = 'Stop'
# Development pipe: never talk to (or collide with) an installed ferrofs service.
$env:FERROFS_DEV_PIPE = 'ferrofs-dev'
$Out = Join-Path $Repo 'assets\screenshots'
. (Join-Path $PSScriptRoot 'e2e-ui-helpers.ps1')
Add-Type @'
using System; using System.IO; using System.Runtime.InteropServices; using Microsoft.Win32.SafeHandles;
public static class Sparse {
  [DllImport("kernel32.dll", SetLastError=true)]
  static extern bool DeviceIoControl(SafeFileHandle h, uint code, IntPtr i, uint il, IntPtr o, uint ol, out uint ret, IntPtr ov);
  // A file of `length` bytes that occupies (almost) no disk space.
  public static void Create(string path, long length) {
    Directory.CreateDirectory(Path.GetDirectoryName(path));
    using (var fs = new FileStream(path, FileMode.Create, FileAccess.ReadWrite)) {
      uint ret; DeviceIoControl(fs.SafeFileHandle, 0x900C4, IntPtr.Zero, 0, IntPtr.Zero, 0, out ret, IntPtr.Zero);
      fs.SetLength(length);
    }
  }
}
'@
$sandbox = Join-Path $env:TEMP 'ferrofs-demo'
$GB = 1GB; $MB = 1MB
function Add-Sparse($rel, $bytes) { [Sparse]::Create((Join-Path $sandbox $rel), [long]$bytes) }
function Add-Random($rel, $bytes, $seed) {
  $path = Join-Path $sandbox $rel
  New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
  $b = [byte[]]::new($bytes); (New-Object Random $seed).NextBytes($b); [IO.File]::WriteAllBytes($path, $b)
}
function Add-Text($rel, $bytes, $text) {
  $path = Join-Path $sandbox $rel
  New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
  $line = [Text.Encoding]::UTF8.GetBytes($text)
  $b = [byte[]]::new($bytes); for ($i = 0; $i -lt $bytes; $i++) { $b[$i] = $line[$i % $line.Length] }
  [IO.File]::WriteAllBytes($path, $b)
}

# --- demo disk ---------------------------------------------------------------------------
if (Test-Path $sandbox) { Remove-Item -Recurse -Force $sandbox }
Add-Sparse 'Games\Starfall\Data\world.pak' (18.4 * $GB); Add-Sparse 'Games\Starfall\Data\textures.pak' (9.1 * $GB)
Add-Sparse 'Games\Starfall\Data\audio.pak' (3.2 * $GB); Add-Sparse 'Games\Starfall\Starfall.exe' (148 * $MB)
Add-Sparse 'Games\Moonlit Racing\content.bin' (11.7 * $GB); Add-Sparse 'Games\Moonlit Racing\movies\intro.bk2' (1.4 * $GB)
Add-Sparse 'Videos\Holiday 2025.mkv' (4.3 * $GB); Add-Sparse 'Videos\Talk recording.mp4' (2.2 * $GB); Add-Sparse 'Videos\Drone clips\clip-01.mp4' (870 * $MB)
Add-Sparse 'Downloads\ubuntu-24.04-desktop-amd64.iso' (5.9 * $GB); Add-Sparse 'Downloads\StudioSuite-setup.exe' (1.3 * $GB)
Add-Sparse 'Downloads\dataset-archive.zip' (3.6 * $GB)
Add-Sparse 'Projects\webshop\node_modules\.pnpm\store.bin' (1.15 * $GB); Add-Text 'Projects\webshop\package.json' 400 '{"name":"webshop"} '
Add-Text 'Projects\webshop\package-lock.json' 4000 '{"lockfileVersion":3} '; Add-Sparse 'Projects\webshop\dist\bundle.js' (14 * $MB)
Add-Sparse 'Projects\engine\target\debug\deps\engine.rlib' (6.4 * $GB); Add-Text 'Projects\engine\Cargo.toml' 300 '[package] name = "engine" '
Add-Text 'Projects\engine\src\main.rs' 9000 'fn main() { println!("engine"); } '
Add-Sparse 'Projects\vision\.venv\Lib\site-packages\torch\lib\torch_cuda.dll' (2.9 * $GB); Add-Text 'Projects\vision\.venv\pyvenv.cfg' 120 'home = C:\Python312 '
Add-Text 'Projects\vision\pyproject.toml' 300 '[project] name = "vision" '; Add-Sparse 'Projects\vision\__pycache__\model.cpython-312.pyc' (3 * $MB)
Add-Sparse 'Users\demo\AppData\Local\Temp\installer-cache.tmp' (1.8 * $GB); Add-Sparse 'Users\demo\AppData\Local\Temp\crash-dump.dmp' (640 * $MB)
Add-Sparse 'Users\demo\AppData\Local\npm-cache\_cacache\content.bin' (2.4 * $GB)
Add-Sparse 'Users\demo\AppData\Local\pip\cache\http\wheels.bin' (1.6 * $GB)
Add-Sparse 'Users\demo\.cargo\registry\cache\crates.bin' (3.1 * $GB)
Add-Sparse 'Windows\SoftwareDistribution\Download\update.cab' (2.7 * $GB); Add-Sparse 'Windows\Temp\setup.log' (420 * $MB)
foreach ($i in 1..12) { Add-Random "Photos\2025\IMG_$(1000 + $i).jpg" (300KB + $i * 37KB) $i }
foreach ($i in 1, 2, 3, 5, 8) { Copy-Item (Join-Path $sandbox "Photos\2025\IMG_$(1000 + $i).jpg") (New-Item -ItemType Directory -Force (Join-Path $sandbox 'Photos\Phone backup')).FullName }
foreach ($i in 2, 8) { Copy-Item (Join-Path $sandbox "Photos\2025\IMG_$(1000 + $i).jpg") (Join-Path $sandbox "Photos\IMG_$(1000 + $i) (copy).jpg") }
foreach ($n in 'Quarterly report 2025.pdf', 'Annual report draft.docx', 'report-figures.xlsx', 'Tax return 2025.pdf', 'Notes.txt') { Add-Random "Documents\$n" (180KB + $n.Length * 900) $n.Length }
Add-Random 'Documents\Archive\old report 2019.pdf' 250KB 3
foreach ($f in 'core.dll', 'render.dll', 'ui.dll', 'plugins\import.dll', 'plugins\export.dll') { Add-Text "Apps\StudioSuite\$f" (9 * $MB) "<symbol name=`"$f`" section=`".text`" flags=`"export`" />" }
Add-Text 'Apps\StudioSuite\resources\strings.xml' (6 * $MB) '<string id="menu.file">File</string>'
Add-Random 'Apps\StudioSuite\resources\splash.png' (2 * $MB) 7

subst $Drive /D 2>$null | Out-Null
subst $Drive $sandbox
New-Item -ItemType Directory -Force $Out | Out-Null
$fake = Start-Process (Join-Path $Repo 'target\debug\examples\fake_service.exe') -ArgumentList "$Drive\" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$storage = Join-Path $Repo 'target\debug\ferro-storage.exe'
$demoEnv = @{
  TEMP = "$Drive\Users\demo\AppData\Local\Temp"; TMP = "$Drive\Users\demo\AppData\Local\Temp"
  LOCALAPPDATA = "$Drive\Users\demo\AppData\Local"; APPDATA = "$Drive\Users\demo\AppData\Roaming"
  USERPROFILE = "$Drive\Users\demo"; WINDIR = "$Drive\Windows"; SystemDrive = $Drive
  ProgramFiles = "$Drive\Program Files"; 'ProgramFiles(x86)' = "$Drive\Program Files (x86)"
  FERROFS_DEV_ANY_SERVER = '1'
}
function Start-Demo($exe, $arguments) {
  $saved = @{}; foreach ($k in $demoEnv.Keys) { $saved[$k] = [Environment]::GetEnvironmentVariable($k); [Environment]::SetEnvironmentVariable($k, $demoEnv[$k]) }
  Remove-Item Env:CARGO_HOME, Env:GRADLE_USER_HOME -ErrorAction SilentlyContinue
  $p = Start-Process $exe -ArgumentList $arguments -PassThru
  foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
  $p
}
try {
  $p = Start-Demo $storage @("$Drive\")
  $h = Wait-Window $p.Id; Start-Sleep -Seconds 2; Shot $h 'analyzer'; Stop-Process -Id $p.Id -Force

  $p = Start-Demo $storage @('--duplicates', "$Drive\Photos")
  $h = Wait-Window $p.Id; Start-Sleep -Seconds 2
  Click-At $h 271 119                       # Mark duplicates
  Shot $h 'duplicates'; Stop-Process -Id $p.Id -Force

  $p = Start-Demo $storage @('--cleanup', "$Drive\Projects")
  $h = Wait-Window $p.Id; Shot $h 'cleanup'; Stop-Process -Id $p.Id -Force

  $p = Start-Demo $storage @('--compression', "$Drive\Apps\StudioSuite")
  $h = Wait-Window $p.Id; Start-Sleep -Seconds 2; Shot $h 'compression'; Stop-Process -Id $p.Id -Force

  $p = Start-Demo (Join-Path $Repo 'target\debug\ferro-search.exe') @('--show')
  $h = Wait-Window $p.Id
  if ([U]::GetForegroundWindow() -ne $h) { [U]::SetForegroundWindow($h) | Out-Null; Start-Sleep -Milliseconds 500 }
  # Keys go only to our own window.
  if ([U]::GetForegroundWindow() -eq $h) {
    (New-Object -ComObject WScript.Shell).SendKeys('report')
    Start-Sleep -Seconds 1
    Shot $h 'search'
  } else { 'search window not in front; skipped' }
  Stop-Process -Id $p.Id -Force
} finally {
  Get-Process ferro-storage, ferro-search -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$Repo*" } | Stop-Process -Force
  Stop-Process -Id $fake.Id -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
  subst $Drive /D
  Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
