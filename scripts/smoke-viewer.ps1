<#
.SYNOPSIS
  Windows counterpart of scripts/smoke-viewer.sh: build Jackstay with
  backend-windows and the D3D11 reference viewer, start the d3d11_source
  example on a private Local Endpoint, and show its frames in the viewer
  through the ABI 0.10 C calls.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1
  powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1 -HoldMs 250 -Frames 12 -Screenshot $env:TEMP\viewer.png
  powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1 -Source window -ResizeEveryMs 1000 -Frames 60

.NOTES
  -Source window captures only the small window the source opens itself.
  -Screenshot saves the viewer's own window (PrintWindow) while it runs; it
  never captures other windows or the screen. Needs an interactive desktop.
#>
param(
  [int]$Frames = 30,
  [int]$HoldMs = 0,
  [ValidateSet('synthetic', 'window')][string]$Source = 'synthetic',
  [string]$Adapter = 'default',
  [int]$ResizeEveryMs = 0,
  [string]$Screenshot = '',
  [int]$ScreenshotDelayMs = 1500,
  [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Invoke-Checked([string]$File, [string[]]$Arguments) {
  & $File @Arguments
  if ($LASTEXITCODE -ne 0) { throw "$File $($Arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

function Find-CMake {
  $found = Get-Command cmake -ErrorAction SilentlyContinue
  if ($found) { return $found.Source }
  $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
  if (Test-Path $vswhere) {
    $vs = & $vswhere -latest -products * -property installationPath
    $bundled = Join-Path $vs 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
    if ($vs -and (Test-Path $bundled)) { return $bundled }
  }
  throw 'cmake not found: install CMake or the Visual Studio C++ CMake tools'
}

if (-not $SkipBuild) {
  # --lib: the cdylib the viewer links; --examples alone does not refresh it.
  Invoke-Checked cargo @('build', '--workspace', '--locked', '--features', 'backend-windows', '--lib', '--examples')
  $cmake = Find-CMake
  Invoke-Checked $cmake @('-S', 'tools/capture-viewer-d3d11', '-B', 'build/viewer-d3d11')
  Invoke-Checked $cmake @('--build', 'build/viewer-d3d11', '--config', 'Debug')
}
$viewer = Join-Path $root 'build\viewer-d3d11\Debug\capture-viewer-d3d11.exe'
$sourceExe = Join-Path $root 'target\debug\examples\d3d11_source.exe'

$name = "jackstay-smoke-$PID-$(Get-Random)"
$scratch = Join-Path ([IO.Path]::GetTempPath()) $name
New-Item -ItemType Directory -Path $scratch | Out-Null
$sourceOut = Join-Path $scratch 'source.out'
$sourceErr = Join-Path $scratch 'source.err'
$viewerOut = Join-Path $scratch 'viewer.out'
$viewerErr = Join-Path $scratch 'viewer.err'

$sourceArgs = @('--endpoint', $name, '--source', $Source, '--adapter', $Adapter, '--clients', '1', '--seconds', '300')
if ($ResizeEveryMs -gt 0) { $sourceArgs += @('--resize-every-ms', "$ResizeEveryMs") }
$sourceProcess = $null
$viewerProcess = $null
try {
  $sourceProcess = Start-Process -FilePath $sourceExe -ArgumentList $sourceArgs -NoNewWindow -PassThru `
    -RedirectStandardOutput $sourceOut -RedirectStandardError $sourceErr
  $null = $sourceProcess.Handle  # keeps the exit code available after exit
  $deadline = (Get-Date).AddSeconds(30)
  while (-not ((Test-Path $sourceOut) -and (Select-String -Path $sourceOut -Pattern '^ready ' -Quiet))) {
    if ($sourceProcess.HasExited) { throw "d3d11_source exited early: $(Get-Content -Raw $sourceErr)" }
    if ((Get-Date) -gt $deadline) { throw 'd3d11_source did not become ready' }
    Start-Sleep -Milliseconds 50
  }
  Write-Host (Select-String -Path $sourceOut -Pattern '^ready ').Line

  $viewerArgs = @('--endpoint', $name, '--frames', "$Frames", '--hold-ms', "$HoldMs")
  $viewerProcess = Start-Process -FilePath $viewer -ArgumentList $viewerArgs -NoNewWindow -PassThru `
    -RedirectStandardOutput $viewerOut -RedirectStandardError $viewerErr
  $null = $viewerProcess.Handle
  if ($Screenshot) {
    Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class JackstayViewerShot {
  [StructLayout(LayoutKind.Sequential)] struct Rect { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr window, out Rect rect);
  [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
  // PW_RENDERFULLCONTENT: include DirectX (flip-model) content.
  public static void Save(IntPtr window, string path) {
    SetProcessDPIAware();
    Rect rect;
    if (!GetWindowRect(window, out rect)) throw new InvalidOperationException("GetWindowRect failed");
    using (var bitmap = new Bitmap(rect.Right - rect.Left, rect.Bottom - rect.Top, PixelFormat.Format32bppArgb))
    using (var graphics = Graphics.FromImage(bitmap)) {
      IntPtr dc = graphics.GetHdc();
      bool ok = PrintWindow(window, dc, 2);
      graphics.ReleaseHdc(dc);
      if (!ok) throw new InvalidOperationException("PrintWindow failed");
      bitmap.Save(path, ImageFormat.Png);
    }
  }
}
'@
    # The viewer prints its own window handle once the window exists.
    $deadline = (Get-Date).AddSeconds(30)
    $line = $null
    while (-not $line) {
      if ($viewerProcess.HasExited) { throw "viewer exited before its window appeared: $(Get-Content -Raw $viewerErr)" }
      if ((Get-Date) -gt $deadline) { throw 'viewer window did not appear' }
      Start-Sleep -Milliseconds 50
      if (Test-Path $viewerOut) { $line = Select-String -Path $viewerOut -Pattern '^viewer window=([0-9A-Fa-f]+)' }
    }
    $handle = [IntPtr][Convert]::ToInt64($line.Matches[0].Groups[1].Value, 16)
    Start-Sleep -Milliseconds $ScreenshotDelayMs
    if ($viewerProcess.HasExited) { throw 'viewer exited before the screenshot; raise -Frames or -HoldMs' }
    [JackstayViewerShot]::Save($handle, [IO.Path]::GetFullPath($Screenshot))
    Write-Host "screenshot: $Screenshot"
  }
  if (-not $viewerProcess.WaitForExit(300000)) { throw 'viewer did not finish' }
  $viewerProcess.WaitForExit()
  Get-Content $viewerOut | Write-Host
  $errors = Get-Content -Raw $viewerErr
  if ($errors) { Write-Host $errors }
  if ($viewerProcess.ExitCode -ne 0) { throw "viewer failed with exit code $($viewerProcess.ExitCode)" }
  if (-not (Select-String -Path $viewerOut -Pattern "^presented_frames=$Frames$" -Quiet)) {
    throw "viewer did not present $Frames frames"
  }
  if (-not $sourceProcess.WaitForExit(30000)) { throw 'd3d11_source did not exit after the viewer disconnected' }
  Get-Content $sourceOut | Write-Host
} finally {
  foreach ($process in @($viewerProcess, $sourceProcess)) {
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id -Force }
  }
  Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
}
