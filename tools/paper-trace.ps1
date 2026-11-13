# Drives one pin drag on the 3-card demo and writes a paper-model CSV trace.
#
# Usage (from the repo root, after `cargo build --release`):
#   pwsh tools\paper-trace.ps1
#   pwsh tools\paper-trace.ps1 -Output target\tall.csv -Damping 0.55 -Wind 0.00002
#
# The coordinates are physical pixels for a 200% DPI display (the development
# machine): the first demo card's pin is at (1614, 87). Pass -PinX/-PinY/
# -TargetX for another layout. The demo never saves pegged.txt, but it still
# loads the real one; isolate/clear it before tracing for a clean 3-card run.

param(
    [string]$Output = "target\paper-trace.csv",
    [string]$App = "target\release\pegline.exe",
    [int]$PinX = 1614,
    [int]$PinY = 87,
    [int]$TargetX = 1814,
    [string]$Damping = "",
    [string]$Drag = "",
    [string]$Wind = "",
    [string]$Aspect = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root $App
$csv = Join-Path $root $Output
$csvDir = Split-Path -Parent $csv

if (-not (Test-Path $exe)) {
    throw "release build not found: $exe (run cargo build --release)"
}
New-Item -ItemType Directory -Force -Path $csvDir | Out-Null
Remove-Item -Force -ErrorAction SilentlyContinue $csv

$env:PEGLINE_ALLOW_SECOND_INSTANCE = "1"
$env:PEGLINE_PAPER_TRACE = $csv
if ($Damping) { $env:PEGLINE_PAPER_DAMPING = $Damping }
if ($Drag) { $env:PEGLINE_PAPER_DRAG = $Drag }
if ($Wind) { $env:PEGLINE_PAPER_WIND = $Wind }
if ($Aspect) { $env:PEGLINE_PAPER_ASPECT = $Aspect }

if (-not ("PaperTraceMouse" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class PaperTraceMouse {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
}
"@
}
[PaperTraceMouse]::SetProcessDPIAware() | Out-Null

$process = Start-Process -FilePath $exe -ArgumentList "--demo" -WorkingDirectory $root -PassThru
try {
    # The demo drops the line for 5 s on first run; 3.4 s is enough on the
    # development machine after the demo assets are warm.
    Start-Sleep -Milliseconds 3400
    [PaperTraceMouse]::SetCursorPos($PinX, $PinY) | Out-Null
    Start-Sleep -Milliseconds 250
    [PaperTraceMouse]::mouse_event(0x02, 0, 0, 0, [UIntPtr]::Zero) # left down
    Start-Sleep -Milliseconds 200
    $steps = 10
    for ($i = 1; $i -le $steps; $i++) {
        $x = [int]($PinX + (($TargetX - $PinX) * $i / $steps))
        [PaperTraceMouse]::SetCursorPos($x, $PinY) | Out-Null
        Start-Sleep -Milliseconds 45
    }
    [PaperTraceMouse]::mouse_event(0x04, 0, 0, 0, [UIntPtr]::Zero) # left up
    Start-Sleep -Milliseconds 2500
}
finally {
    if (-not $process.HasExited) {
        Stop-Process -Id $process.Id -Force
    }
    $process.WaitForExit()
}

if (-not (Test-Path $csv)) {
    throw "the app did not write a trace; check %APPDATA%\Pegline\error.log"
}
Write-Host "trace: $csv"
